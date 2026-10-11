#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Suggests the smallest set of code owners that covers every file in a commit.

Gets each changed file's owners from the Gerrit code-owners plugin, then
searches for the fewest owners who together cover every file. When several
sets are equally small, it prefers owners from closer OWNERS files. Pass --all
to see every smallest set.

If more than 5 reviewers are needed, it points to the docs for asking for an
owners override.

Ties between equally good owners are broken randomly to spread out review
load. The randomness is seeded by today's date and the commit author, so
repeated runs on the same day give the same answer. (Very large changes fall
back to a simpler search that doesn't do this.)

Root OWNERS members can approve anything, so they'd always be picked. To avoid
that, they're only used for files that have no other owners, unless you pass
--include-root. Owners whose calendar says they're out of office, and who won't
be back within 4 hours, are skipped the same way. That check uses the prebuilt
`luci-auth` (or one on PATH), which works without logging in on cloudtops and
needs `luci-auth login -scopes-gerrit` elsewhere; without it, the check is
skipped with a note.
"""

import argparse
import collections
import concurrent.futures
import datetime
import json
import os
import random
import re
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import AbstractSet, Any, Callable, Mapping, TypeVar

# Gerrit prefixes JSON responses with this to prevent XSSI.
XSSI_PREFIX = ")]}'"

# A nonexistent top-level path only matches root OWNERS, so its owners are
# exactly the root owners.
ROOT_PROBE_PATH = "__min_owners_nonexistent__"

HTTP_TIMEOUT_SECS = 30

GERRIT_OAUTH_SCOPE = "https://www.googleapis.com/auth/gerritcodereview"

# Pinned in //manifests/prebuilts. This file is in //tools/devshell/contrib.
PREBUILT_LUCI_AUTH = os.path.join(
    os.path.dirname(os.path.abspath(__file__)),
    "../../../prebuilt/tools/luci-auth/luci-auth",
)

# Keeps availability request URLs well under Gerrit's URL length limit.
AVAILABILITY_BATCH_SIZE = 100

# Short OOO blocks (appointments, errands) shouldn't stop someone from getting
# a review they'll see later the same day.
OOO_GRACE_SECS = 4 * 60 * 60

# Past this many reviewers, collecting approvals is enough of a burden that an
# owners override is worth considering, per the OWNERS docs.
OVERRIDE_SUGGESTION_THRESHOLD = 5
OWNERS_OVERRIDE_DOCS = (
    "https://fuchsia.dev/fuchsia-src/development/source_code/owners"
    "#owners-override"
)

Getter = Callable[[str], Any]
T = TypeVar("T")


def git(*args: str) -> str:
    return subprocess.check_output(["git", *args], text=True).strip()


def parse_name_status(out: str) -> list[str]:
    """Returns the paths in `git diff --name-status -z` output."""
    files: set[str] = set()
    fields = iter(out.split("\0"))
    for status in fields:
        if not status:
            continue
        files.add(next(fields))
        # Renames and copies list a second path. Both need owner approval.
        if status[0] in "RC":
            files.add(next(fields))
    return sorted(files)


def diff_args(rev: str) -> list[str]:
    """Returns the `git diff` revision args for a commit or range."""
    if ".." not in rev:
        # Gerrit diffs a commit against its first parent, which also gives
        # a useful file list for merge commits (unlike `rev^!`).
        return [f"{rev}^1", rev]
    # Use the merge base so that `main..HEAD` only counts this branch's
    # changes, even if local main has moved on since the branch was cut.
    base, tip = re.split(r"\.\.\.?", rev, maxsplit=1)
    return [f"{base or 'HEAD'}...{tip or 'HEAD'}"]


def changed_files(rev: str) -> list[str]:
    out = subprocess.check_output(
        ["git", "diff", "--name-status", "-M", "-z", *diff_args(rev)], text=True
    )
    return parse_name_status(out)


def parse_remote_url(url: str) -> tuple[str, str]:
    """Returns the Gerrit review host URL and project for a git remote URL."""
    # Matches e.g. https://fuchsia.googlesource.com/fuchsia and the short
    # sso://turquoise-internal/vendor/google form.
    m = re.match(
        r"(?:https?|sso|rpc)://([\w-]+)"
        r"(?:\.(?:git\.corp\.google|googlesource)\.com)?"
        r"/(?:a/)?(.+?)(?:\.git)?/?$",
        url,
    )
    if not m:
        raise ValueError(f"Can't figure out the Gerrit host from {url!r}")
    return f"https://{m.group(1)}-review.googlesource.com", m.group(2)


def parse_json(body: str) -> Any:
    if not body.startswith(XSSI_PREFIX):
        raise RuntimeError(f"Unexpected Gerrit response: {body[:200]}")
    return json.loads(body[len(XSSI_PREFIX) :])


def gerrit_getter(host: str) -> Getter:
    """Returns a function that GETs a Gerrit REST path and returns its JSON.

    Public hosts like fuchsia-review work anonymously. Private hosts like
    turquoise-internal-review redirect anonymous requests to a login page or
    reject them, so those go through gob-curl, which handles SSO auth on the
    /a/ prefix.
    """

    def anonymous(path: str) -> Any:
        url = host + path
        try:
            with urllib.request.urlopen(url, timeout=HTTP_TIMEOUT_SECS) as resp:
                return parse_json(resp.read().decode())
        except urllib.error.URLError as e:
            raise RuntimeError(f"GET {url} failed: {e}") from e

    def authenticated(path: str) -> Any:
        url = f"{host}/a{path}"
        proc = subprocess.run(
            ["gob-curl", "--silent", "--show-error", "--fail", url],
            capture_output=True,
            text=True,
        )
        if proc.returncode:
            raise RuntimeError(f"gob-curl {url} failed: {proc.stderr[:200]}")
        return parse_json(proc.stdout)

    probe = host + "/config/server/version"
    try:
        with urllib.request.urlopen(probe, timeout=HTTP_TIMEOUT_SECS) as resp:
            # Hosts that need auth either redirect anonymous requests to a
            # login page or reject them outright.
            needs_auth = resp.url != probe
    except urllib.error.HTTPError as e:
        e.close()
        if e.code not in (401, 403):
            raise
        needs_auth = True
    if not needs_auth:
        return anonymous
    if not shutil.which("gob-curl"):
        sys.exit(f"{host} needs auth, but gob-curl isn't on PATH")
    return authenticated


def fetch_owners(
    get: Getter,
    project: str,
    branch: str,
    path: str,
    account_ids: dict[str, int] | None = None,
) -> dict[str, int] | None:
    """Returns {owner email: distance} for a path, or None if anyone owns it.

    Also adds each owner's Gerrit account ID to `account_ids`, if given.
    """
    data = get(
        "/projects/{}/branches/{}/code_owners/{}?o=DETAILS&limit=1000".format(
            urllib.parse.quote(project, safe=""),
            urllib.parse.quote(branch, safe=""),
            urllib.parse.quote(path, safe=""),
        )
    )
    if data.get("owned_by_all_users"):
        return None
    owners: dict[str, int] = {}
    for o in data.get("code_owners", []):
        account = o["account"]
        if "email" not in account:
            continue
        owners[account["email"]] = o.get("scorings", {}).get("DISTANCE", 0)
        if account_ids is not None and "_account_id" in account:
            account_ids[account["email"]] = account["_account_id"]
    return owners


def luci_auth_token() -> str:
    # Fall back to PATH for checkouts that haven't fetched the prebuilt yet.
    luci_auth = (
        PREBUILT_LUCI_AUTH
        if os.path.exists(PREBUILT_LUCI_AUTH)
        else shutil.which("luci-auth")
    )
    if not luci_auth:
        raise RuntimeError("luci-auth isn't in prebuilt/tools or on PATH")
    proc = subprocess.run(
        [luci_auth, "token", "-scopes", GERRIT_OAUTH_SCOPE],
        capture_output=True,
        text=True,
    )
    if proc.returncode:
        msg = proc.stderr.strip().splitlines()[-1:] or ["unknown error"]
        raise RuntimeError(f"luci-auth token failed: {msg[0]}")
    return proc.stdout.strip()


def out_of_office(
    host: str,
    account_ids: Mapping[str, int],
    token: str,
    now: float | None = None,
) -> set[str]:
    """Returns the emails of accounts whose calendar says they're OOO.

    People who'll be back within OOO_GRACE_SECS don't count. This uses
    Gerrit's Google-only availability plugin, which needs a Gaia OAuth token.
    gob-curl's credentials don't work for it.
    """
    if now is None:
        now = time.time()
    emails = {i: e for e, i in account_ids.items()}
    ids = sorted(emails)
    ooo: set[str] = set()
    for start in range(0, len(ids), AVAILABILITY_BATCH_SIZE):
        query = urllib.parse.urlencode(
            [("id", i) for i in ids[start : start + AVAILABILITY_BATCH_SIZE]]
        )
        req = urllib.request.Request(
            f"{host}/a/plugins/availability/statuses/?{query}",
            headers={"Authorization": f"Bearer {token}"},
        )
        try:
            with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT_SECS) as resp:
                statuses = parse_json(resp.read().decode())
            for s in statuses:
                # OUTSIDE_WORKING_HOURS is ignored on purpose: it's often just
                # a different time zone, and they'll be back within a day.
                if s["status"] != "OUT_OF_OFFICE":
                    continue
                back = s.get("return_time", {}).get("seconds")
                if back is not None and back - now < OOO_GRACE_SECS:
                    continue
                ooo.add(emails[s["account_id"]])
        # This check is best-effort, so any failure (including timeouts,
        # which aren't URLErrors, and odd responses) just turns it off.
        except (OSError, RuntimeError, ValueError, LookupError, TypeError) as e:
            raise RuntimeError(f"availability lookup failed: {e!r}") from e
    return ooo


def prefer(
    owners: dict[str, int], keep: Callable[[str], bool]
) -> dict[str, int]:
    """Returns the owners that pass `keep`, or all of them if none do."""
    return {o: d for o, d in owners.items() if keep(o)} or owners


def owners_to_cover(
    file_owners: Mapping[str, Mapping[str, int] | None],
    root_owners: set[str],
    author: str,
    include_root: bool,
    unavailable: AbstractSet[str] = frozenset(),
) -> dict[str, dict[str, int]]:
    """Drops files that need no extra approval, and owners we'd rather skip.

    Unavailable owners, and root owners unless `include_root` is set, are
    only kept for files that nobody else can approve.
    """
    result: dict[str, dict[str, int]] = {}
    for f, owners in file_owners.items():
        # None means anyone can approve, and the author implicitly approves
        # files they own, so neither needs another reviewer.
        if owners is None or author in owners:
            continue
        # Availability is checked first because an available root owner is a
        # better pick than one who's out of office.
        kept = prefer(dict(owners), lambda o: o not in unavailable)
        if not include_root:
            kept = prefer(kept, lambda o: o not in root_owners)
        result[f] = kept
    return result


def plural(n: int, noun: str) -> str:
    return f"{n} {noun}" if n == 1 else f"{n} {noun}s"


def owner_distances(file_owners: dict[str, dict[str, int]]) -> dict[str, int]:
    """Returns each owner's total distance over the files they own."""
    distance: dict[str, int] = collections.defaultdict(int)
    for owners in file_owners.values():
        for owner, dist in owners.items():
            distance[owner] += dist
    return distance


def greedy_cover(
    file_owners: dict[str, dict[str, int]],
) -> tuple[list[tuple[str, list[str]]], list[str]]:
    """Picks owners until every file is covered.

    Returns the picked owners with the files each one covers, plus any files
    that have no owners at all.
    """
    uncovered = set(file_owners)
    picks = []
    while uncovered:
        covers: dict[str, list[str]] = collections.defaultdict(list)
        distance: dict[str, int] = collections.defaultdict(int)
        for f in uncovered:
            for owner, dist in file_owners[f].items():
                covers[owner].append(f)
                distance[owner] += dist
        if not covers:
            break
        owner = min(covers, key=lambda o: (-len(covers[o]), distance[o], o))
        picks.append((owner, sorted(covers[owner])))
        uncovered -= set(covers[owner])
    return picks, sorted(uncovered)


# A group of interchangeable owners (they cover exactly the same files), and
# the files they cover.
OwnerGroup = tuple[list[str], list[str]]


def all_min_covers(
    file_owners: dict[str, dict[str, int]],
    max_steps: int = 200_000,
) -> list[list[OwnerGroup]] | None:
    """Returns every smallest set of owners that covers all ownable files.

    Owners who cover exactly the same files are interchangeable, so they're
    merged into one group. Otherwise a few directories with several owners
    each would multiply into hundreds of near-identical answers. Each answer
    is a list of groups; picking any one owner from each group gives a
    minimal set.

    Finding the minimum is NP-hard, so this gives up and returns None after
    `max_steps` search steps. That only happens for very large changes.
    """
    covers: dict[str, set[str]] = collections.defaultdict(set)
    for f, owners in file_owners.items():
        for owner in owners:
            covers[owner].add(f)
    distance = owner_distances(file_owners)

    group_owners: dict[frozenset[str], list[str]] = collections.defaultdict(
        list
    )
    for owner, owned in covers.items():
        group_owners[frozenset(owned)].append(owner)
    groups = list(group_owners)
    groups_for_file: dict[str, list[int]] = collections.defaultdict(list)
    for i, group in enumerate(groups):
        for f in group:
            groups_for_file[f].append(i)

    # The greedy answer is an upper bound on the minimum size, which prunes
    # most of the search.
    best = len(greedy_cover(file_owners)[0])
    found: set[frozenset[int]] = set()
    largest = max((len(g) for g in groups), default=1)
    steps = 0

    class TooBig(Exception):
        pass

    def search(uncovered: frozenset[str], chosen: frozenset[int]) -> None:
        nonlocal best, steps
        steps += 1
        if steps > max_steps:
            raise TooBig()
        if not uncovered:
            if len(chosen) < best:
                best = len(chosen)
                found.clear()
            found.add(chosen)
            return
        # Each extra group covers at most `largest` files.
        if len(chosen) + -(-len(uncovered) // largest) > best:
            return
        # Branch on the file with the fewest options to keep the tree small.
        f = min(uncovered, key=lambda f: len(groups_for_file[f]))
        for i in groups_for_file[f]:
            search(uncovered - groups[i], chosen | {i})

    ownable = frozenset(f for f, owners in file_owners.items() if owners)
    try:
        search(ownable, frozenset())
    # The recursion depth is bounded by the greedy answer's size, so it only
    # gets too deep for changes that would need hundreds of reviewers.
    except (TooBig, RecursionError):
        return None

    def owner_key(o: str) -> tuple[int, str]:
        return distance[o], o

    results = [
        sorted(
            (sorted(group_owners[groups[i]], key=owner_key), sorted(groups[i]))
            for i in cover
        )
        for cover in found
    ]
    # Show answers with closer owners first.
    results.sort(
        key=lambda cover: (
            sum(distance[owners[0]] for owners, _ in cover),
            cover,
        )
    )
    return results


def pick_reviewers(
    covers: list[list[OwnerGroup]],
    distance: Mapping[str, int],
    rng: random.Random,
) -> list[tuple[str, list[str]]]:
    """Picks one owner from each group of one of the smallest covers.

    Closer owners still win, but ties are broken randomly so the same few
    people (e.g. whoever sorts first alphabetically) don't get every review.
    """

    def closest(items: list[T], dist: Callable[[T], int]) -> T:
        best = min(dist(x) for x in items)
        return rng.choice([x for x in items if dist(x) == best])

    cover = closest(
        covers, lambda c: sum(distance[owners[0]] for owners, _ in c)
    )
    picks = [
        (closest(owners, lambda o: distance[o]), covered)
        for owners, covered in cover
    ]
    return sorted(picks, key=lambda p: (-len(p[1]), p[0]))


def seeded_rng(author: str) -> random.Random:
    # Seeding with the date keeps reruns stable within a day, and the author
    # stops everyone's changes from going to the same people on a given day.
    return random.Random(f"{datetime.date.today().isoformat()} {author}")


def suggest(
    file_owners: dict[str, dict[str, int]], author: str
) -> tuple[list[tuple[str, list[str]]], list[list[OwnerGroup]] | None]:
    """Returns the suggested owners, plus every smallest cover if known."""
    covers = all_min_covers(file_owners)
    if covers is None:
        return greedy_cover(file_owners)[0], None
    rng = seeded_rng(author)
    return pick_reviewers(covers, owner_distances(file_owners), rng), covers


def main() -> None:
    parser = argparse.ArgumentParser(
        prog="fx min-owners",
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "rev",
        nargs="?",
        default="HEAD",
        help="commit, or range like main..HEAD (default: HEAD)",
    )
    parser.add_argument(
        "--branch",
        default="main",
        help="branch whose OWNERS files to use (default: main)",
    )
    parser.add_argument(
        "--include-root",
        action="store_true",
        help="consider root OWNERS members for every file, not just as a fallback",
    )
    parser.add_argument(
        "--author",
        help="email that gets implicit approval (default: the commit author); "
        "set this if your git email differs from your Gerrit account's",
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="store_true",
        help="list the files each owner covers",
    )
    parser.add_argument(
        "--all",
        action="store_true",
        help="list every smallest set of owners instead of just one",
    )
    args = parser.parse_args()

    try:
        run(args)
    except (
        RuntimeError,
        ValueError,
        urllib.error.URLError,
        subprocess.CalledProcessError,
    ) as e:
        sys.exit(f"Error: {e}")


def run(args: argparse.Namespace) -> None:
    files = changed_files(args.rev)
    if not files:
        sys.exit("No changed files.")
    if any(f == "OWNERS" or f.endswith("/OWNERS") for f in files):
        print(
            f"Warning: owners come from OWNERS files on the '{args.branch}' "
            "branch, so this commit's own OWNERS changes aren't taken into "
            "account.\n",
            file=sys.stderr,
        )
    host, project = parse_remote_url(git("remote", "get-url", "origin"))
    tip = re.split(r"\.\.\.?", args.rev)[-1] or "HEAD"
    author = args.author or git("log", "-1", "--format=%ae", tip)

    get = gerrit_getter(host)
    # Written from several threads, which is fine because each email always
    # maps to the same ID.
    account_ids: dict[str, int] = {}
    with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
        results = list(
            pool.map(
                lambda p: fetch_owners(
                    get, project, args.branch, p, account_ids
                ),
                files + [ROOT_PROBE_PATH],
            )
        )
    *results, root = results
    root_owners = set(root or {})
    file_owners = dict(zip(files, results))

    to_cover = owners_to_cover(
        file_owners, root_owners, author, args.include_root
    )
    # Check everyone who could approve, not just the preferred owners, since
    # root owners become the fallback if all the others are out.
    to_check = {
        o: account_ids[o]
        for f in to_cover
        for o in file_owners[f] or {}
        if o in account_ids
    }
    ooo: set[str] = set()
    if to_check:
        try:
            ooo = out_of_office(host, to_check, luci_auth_token())
        except RuntimeError as e:
            print(
                f"Note: couldn't check who's out of office: {e}\n",
                file=sys.stderr,
            )
    skipped_ooo: list[str] = []
    if ooo:
        before, _ = suggest(to_cover, author)
        to_cover = owners_to_cover(
            file_owners, root_owners, author, args.include_root, ooo
        )
        listed = {o for owners in to_cover.values() for o in owners}
        skipped_ooo = sorted(
            o for o, _ in before if o in ooo and o not in listed
        )
    picks, covers = suggest(to_cover, author)
    unowned = sorted(f for f, owners in to_cover.items() if not owners)

    skipped = len(files) - len(to_cover)
    if skipped:
        verb = "needs" if skipped == 1 else "need"
        print(
            f"({plural(skipped, 'file')} {verb} no extra owner: owned by "
            f"{author} or by everyone)\n"
        )

    def describe(owner: str) -> str:
        if owner in root_owners:
            owner += " [root]"
        if owner in ooo:
            owner += " [OOO]"
        return owner

    if covers is None:
        print(
            "Note: too many files to find the true minimum, so this list may "
            "have more reviewers than needed.\n",
            file=sys.stderr,
        )
    if covers is not None and args.all:
        if covers[0]:
            print(f"The minimum number of owners is {len(covers[0])}.")
            if len(covers) == 1:
                print("Pick one owner from each group.")
            else:
                print(
                    f"There are {len(covers)} options; pick one owner from "
                    "each group of any option."
                )
        for n, cover in enumerate(covers, 1):
            if not cover:
                continue
            print(f"\nOption {n}:" if len(covers) > 1 else "")
            for owners, covered in cover:
                if len(owners) == 1:
                    print(f"  - {plural(len(covered), 'file')}:")
                else:
                    print(f"  - {plural(len(covered), 'file')}, any one of:")
                for owner in owners:
                    print(f"      {describe(owner)}")
                if args.verbose:
                    print("    Files:")
                    for f in covered:
                        print(f"      {f}")
    else:
        for owner, covered in picks:
            print(f"{describe(owner)}: {plural(len(covered), 'file')}")
            if args.verbose:
                for f in covered:
                    print(f"    {f}")
    if unowned:
        print("\nNo owners found for:")
        for f in unowned:
            print(f"    {f}")
    if skipped_ooo:
        print("\nSkipped because they're out of office:")
        for owner in skipped_ooo:
            print(f"    {owner}")
    if len(picks) > OVERRIDE_SUGGESTION_THRESHOLD:
        print(
            f"\nThat's {plural(len(picks), 'reviewer')}. If this change is "
            "mostly mechanical, consider\nasking for an owners override "
            "instead. See:\n"
            f"    {OWNERS_OVERRIDE_DOCS}"
        )


if __name__ == "__main__":
    main()
