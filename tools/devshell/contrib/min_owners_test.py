#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import inspect
import io
import json
import os
import random
import shutil
import subprocess
import sys
import unittest
import urllib.error
import urllib.parse
import urllib.request
from typing import Any
from unittest import mock

import min_owners


class PluralTest(unittest.TestCase):
    def test_plural(self) -> None:
        self.assertEqual(min_owners.plural(0, "file"), "0 files")
        self.assertEqual(min_owners.plural(1, "file"), "1 file")
        self.assertEqual(min_owners.plural(2, "file"), "2 files")


class ParseNameStatusTest(unittest.TestCase):
    def test_keeps_both_rename_paths(self) -> None:
        out = "M\0src/a.cc\0R100\0src/old.cc\0src/new.cc\0D\0src/tab\tname.cc\0"
        self.assertEqual(
            min_owners.parse_name_status(out),
            ["src/a.cc", "src/new.cc", "src/old.cc", "src/tab\tname.cc"],
        )

    def test_empty(self) -> None:
        self.assertEqual(min_owners.parse_name_status(""), [])


class DiffArgsTest(unittest.TestCase):
    def test_single_commit_uses_first_parent(self) -> None:
        self.assertEqual(min_owners.diff_args("abc123"), ["abc123^1", "abc123"])

    def test_ranges_use_merge_base(self) -> None:
        cases = {
            "main..HEAD": ["main...HEAD"],
            "main...HEAD": ["main...HEAD"],
            "main..": ["main...HEAD"],
        }
        for rev, want in cases.items():
            with self.subTest(rev=rev):
                self.assertEqual(min_owners.diff_args(rev), want)


class FetchOwnersTest(unittest.TestCase):
    def test_owners(self) -> None:
        requested = []

        def get(path: str) -> Any:
            requested.append(path)
            return {
                "code_owners": [
                    {
                        "account": {
                            "_account_id": 1,
                            "email": "alice@example.com",
                        },
                        "scorings": {"DISTANCE": 1},
                    },
                    {"account": {"email": "bob@example.com"}},
                    {"account": {"_account_id": 123}},
                ]
            }

        account_ids: dict[str, int] = {}
        self.assertEqual(
            min_owners.fetch_owners(
                get, "vendor/google", "main", "a/b c.cc", account_ids
            ),
            {"alice@example.com": 1, "bob@example.com": 0},
        )
        self.assertEqual(account_ids, {"alice@example.com": 1})
        self.assertEqual(
            requested,
            [
                "/projects/vendor%2Fgoogle/branches/main/code_owners/"
                "a%2Fb%20c.cc?o=DETAILS&limit=1000"
            ],
        )

    def test_owned_by_all_users(self) -> None:
        self.assertIsNone(
            min_owners.fetch_owners(
                lambda _: {"owned_by_all_users": True}, "p", "main", "f"
            )
        )


class ParseRemoteUrlTest(unittest.TestCase):
    def test_urls(self) -> None:
        cases = {
            "sso://fuchsia/fuchsia": (
                "https://fuchsia-review.googlesource.com",
                "fuchsia",
            ),
            "https://fuchsia.googlesource.com/fuchsia": (
                "https://fuchsia-review.googlesource.com",
                "fuchsia",
            ),
            "https://fuchsia.googlesource.com/a/infra/recipes.git": (
                "https://fuchsia-review.googlesource.com",
                "infra/recipes",
            ),
            "sso://turquoise-internal/vendor/google": (
                "https://turquoise-internal-review.googlesource.com",
                "vendor/google",
            ),
            "https://turquoise-internal.git.corp.google.com/vendor/google/": (
                "https://turquoise-internal-review.googlesource.com",
                "vendor/google",
            ),
        }
        for url, want in cases.items():
            with self.subTest(url=url):
                self.assertEqual(min_owners.parse_remote_url(url), want)

    def test_unknown_url(self) -> None:
        with self.assertRaises(ValueError):
            min_owners.parse_remote_url("git@github.com:foo/bar.git")


class GerritGetterTest(unittest.TestCase):
    def test_unauthorized_probe_uses_gob_curl(self) -> None:
        def urlopen(url: str, timeout: int) -> Any:
            raise urllib.error.HTTPError(url, 401, "Unauthorized", {}, None)  # type: ignore[arg-type]

        run = mock.Mock(
            return_value=subprocess.CompletedProcess(
                [], 0, stdout=min_owners.XSSI_PREFIX + "{}", stderr=""
            )
        )
        with mock.patch.object(
            urllib.request, "urlopen", urlopen
        ), mock.patch.object(
            shutil, "which", return_value="/bin/gob-curl"
        ), mock.patch.object(
            subprocess, "run", run
        ):
            get = min_owners.gerrit_getter("https://host")
            self.assertEqual(get("/foo"), {})
        self.assertIn("https://host/a/foo", run.call_args.args[0])

    def test_other_probe_errors_propagate(self) -> None:
        def urlopen(url: str, timeout: int) -> Any:
            raise urllib.error.HTTPError(url, 500, "Oops", {}, None)  # type: ignore[arg-type]

        with mock.patch.object(urllib.request, "urlopen", urlopen):
            with self.assertRaises(urllib.error.HTTPError):
                min_owners.gerrit_getter("https://host")


class OwnersToCoverTest(unittest.TestCase):
    def test_filters(self) -> None:
        file_owners = {
            "anyone.md": None,
            "authors.cc": {"author@example.com": 1, "bob@example.com": 1},
            "root_only.cc": {"root@example.com": 1},
            "mixed.cc": {"alice@example.com": 1, "root@example.com": 2},
        }
        self.assertEqual(
            min_owners.owners_to_cover(
                file_owners,
                root_owners={"root@example.com"},
                author="author@example.com",
                include_root=False,
            ),
            {
                # Root owners are a fallback when nobody else owns the file.
                "root_only.cc": {"root@example.com": 1},
                "mixed.cc": {"alice@example.com": 1},
            },
        )

    def test_include_root(self) -> None:
        file_owners = {
            "mixed.cc": {"alice@example.com": 1, "root@example.com": 2}
        }
        self.assertEqual(
            min_owners.owners_to_cover(
                file_owners,
                root_owners={"root@example.com"},
                author="author@example.com",
                include_root=True,
            ),
            file_owners,
        )

    def test_root_owner_author_needs_no_reviewers(self) -> None:
        # Code-owners lists root owners for every file (unless an OWNERS file
        # sets noparent), so a root owner's implicit approval covers it all.
        file_owners = {
            "a.cc": {"alice@example.com": 1, "root@example.com": 2},
            "b.cc": {"bob@example.com": 1, "root@example.com": 3},
        }
        self.assertEqual(
            min_owners.owners_to_cover(
                file_owners,
                root_owners={"root@example.com"},
                author="root@example.com",
                include_root=False,
            ),
            {},
        )

    def test_skips_unavailable_owners(self) -> None:
        file_owners = {
            "some_out.cc": {"alice@example.com": 1, "bob@example.com": 1},
            # An available root owner beats an unavailable regular owner.
            "use_root.cc": {"alice@example.com": 1, "root@example.com": 2},
            # If everyone's out, someone still has to review.
            "all_out.cc": {"alice@example.com": 1},
        }
        self.assertEqual(
            min_owners.owners_to_cover(
                file_owners,
                root_owners={"root@example.com"},
                author="author@example.com",
                include_root=False,
                unavailable={"alice@example.com"},
            ),
            {
                "some_out.cc": {"bob@example.com": 1},
                "use_root.cc": {"root@example.com": 2},
                "all_out.cc": {"alice@example.com": 1},
            },
        )


class GreedyCoverTest(unittest.TestCase):
    def test_prefers_owner_covering_most_files(self) -> None:
        picks, unowned = min_owners.greedy_cover(
            {
                "a/1": {"alice@example.com": 1, "bob@example.com": 1},
                "a/2": {"alice@example.com": 1},
                "b/1": {"alice@example.com": 2, "carol@example.com": 1},
                "c/1": {"dave@example.com": 1},
            }
        )
        self.assertEqual(
            picks,
            [
                ("alice@example.com", ["a/1", "a/2", "b/1"]),
                ("dave@example.com", ["c/1"]),
            ],
        )
        self.assertEqual(unowned, [])

    def test_ties_go_to_closer_owner(self) -> None:
        picks, _ = min_owners.greedy_cover(
            {
                "a/1": {"alice@example.com": 2, "bob@example.com": 1},
                "a/2": {"alice@example.com": 2, "bob@example.com": 1},
            }
        )
        self.assertEqual(picks, [("bob@example.com", ["a/1", "a/2"])])

    def test_unowned_files(self) -> None:
        picks, unowned = min_owners.greedy_cover(
            {"a": {"alice@example.com": 1}, "orphan": {}}
        )
        self.assertEqual(picks, [("alice@example.com", ["a"])])
        self.assertEqual(unowned, ["orphan"])


class AllMinCoversTest(unittest.TestCase):
    def test_groups_interchangeable_owners(self) -> None:
        covers = min_owners.all_min_covers(
            {
                "a/1": {"alice@example.com": 1, "bob@example.com": 2},
                "b/1": {"carol@example.com": 1, "dave@example.com": 1},
            }
        )
        self.assertEqual(
            covers,
            [
                [
                    (["alice@example.com", "bob@example.com"], ["a/1"]),
                    (["carol@example.com", "dave@example.com"], ["b/1"]),
                ]
            ],
        )

    def test_finds_covers_greedy_misses(self) -> None:
        # Greedy grabs alice first (3 files) and then needs two more owners.
        # bob + carol cover everything with two.
        file_owners = {
            "1": {"alice@example.com": 1, "bob@example.com": 1},
            "2": {"alice@example.com": 1, "bob@example.com": 1},
            "3": {"alice@example.com": 1, "carol@example.com": 1},
            "4": {"bob@example.com": 1},
            "5": {"carol@example.com": 1},
        }
        picks, _ = min_owners.greedy_cover(file_owners)
        self.assertEqual(len(picks), 3)
        self.assertEqual(
            min_owners.all_min_covers(file_owners),
            [
                [
                    (["bob@example.com"], ["1", "2", "4"]),
                    (["carol@example.com"], ["3", "5"]),
                ]
            ],
        )

    def test_multiple_options_closest_first(self) -> None:
        covers = min_owners.all_min_covers(
            {
                "1": {"far@example.com": 5, "x@example.com": 1},
                "2": {"far@example.com": 5, "y@example.com": 1},
                "3": {"near@example.com": 1, "x@example.com": 1},
                "4": {"near@example.com": 1, "y@example.com": 1},
            }
        )
        self.assertEqual(
            covers,
            [
                # Total distance 4.
                [
                    (["x@example.com"], ["1", "3"]),
                    (["y@example.com"], ["2", "4"]),
                ],
                # Total distance 12.
                [
                    (["far@example.com"], ["1", "2"]),
                    (["near@example.com"], ["3", "4"]),
                ],
            ],
        )

    def test_unowned_and_empty(self) -> None:
        self.assertEqual(
            min_owners.all_min_covers(
                {"a": {"alice@example.com": 1}, "orphan": {}}
            ),
            [[(["alice@example.com"], ["a"])]],
        )
        self.assertEqual(min_owners.all_min_covers({}), [[]])

    def test_gives_up_past_budget(self) -> None:
        file_owners = {
            str(i): {f"o{i}@example.com": 1, f"o{i + 1}@example.com": 1}
            for i in range(20)
        }
        self.assertIsNone(min_owners.all_min_covers(file_owners, max_steps=10))
        self.assertIsNotNone(min_owners.all_min_covers(file_owners))

    def test_gives_up_on_deep_recursion(self) -> None:
        # No owner covers more than one file, so the search goes one level
        # deeper per file.
        file_owners = {
            str(i): {f"o{i}@example.com": 1, f"p{i}@example.com": 1}
            for i in range(100)
        }
        old = sys.getrecursionlimit()
        # Leave room for the test runner's frames but not for 100 levels.
        sys.setrecursionlimit(len(inspect.stack(0)) + 50)
        try:
            self.assertIsNone(min_owners.all_min_covers(file_owners))
        finally:
            sys.setrecursionlimit(old)


class PickReviewersTest(unittest.TestCase):
    def test_closer_owner_always_wins(self) -> None:
        file_owners = {
            "1": {"near@example.com": 1, "far@example.com": 2},
        }
        covers = min_owners.all_min_covers(file_owners)
        assert covers is not None
        distance = min_owners.owner_distances(file_owners)
        for seed in range(20):
            self.assertEqual(
                min_owners.pick_reviewers(
                    covers, distance, random.Random(seed)
                ),
                [("near@example.com", ["1"])],
            )

    def test_prefers_closer_cover(self) -> None:
        file_owners = {
            "1": {"far@example.com": 5, "x@example.com": 1},
            "2": {"far@example.com": 5, "y@example.com": 1},
            "3": {"near@example.com": 1, "x@example.com": 1},
            "4": {"near@example.com": 1, "y@example.com": 1},
        }
        covers = min_owners.all_min_covers(file_owners)
        assert covers is not None
        distance = min_owners.owner_distances(file_owners)
        for seed in range(20):
            picks = min_owners.pick_reviewers(
                covers, distance, random.Random(seed)
            )
            self.assertEqual(
                [o for o, _ in picks], ["x@example.com", "y@example.com"]
            )

    def test_ties_are_random_but_seeded(self) -> None:
        tied = [f"o{i}@example.com" for i in range(5)]
        file_owners = {"1": {o: 1 for o in tied}}
        covers = min_owners.all_min_covers(file_owners)
        assert covers is not None
        distance = min_owners.owner_distances(file_owners)

        def pick(seed: str) -> str:
            picks = min_owners.pick_reviewers(
                covers, distance, random.Random(seed)
            )
            return picks[0][0]

        self.assertEqual(pick("a"), pick("a"))
        self.assertEqual(
            {pick(str(seed)) for seed in range(50)},
            set(tied),
        )

    def test_picks_among_equally_close_options(self) -> None:
        file_owners = {
            "1": {"a@example.com": 1, "x@example.com": 1},
            "2": {"a@example.com": 1, "y@example.com": 1},
            "3": {"b@example.com": 1, "x@example.com": 1},
            "4": {"b@example.com": 1, "y@example.com": 1},
        }
        covers = min_owners.all_min_covers(file_owners)
        assert covers is not None
        self.assertEqual(len(covers), 2)
        distance = min_owners.owner_distances(file_owners)
        picked = {
            tuple(
                o
                for o, _ in min_owners.pick_reviewers(
                    covers, distance, random.Random(seed)
                )
            )
            for seed in range(50)
        }
        self.assertEqual(
            picked,
            {
                ("a@example.com", "b@example.com"),
                ("x@example.com", "y@example.com"),
            },
        )


class OutOfOfficeTest(unittest.TestCase):
    def test_only_out_of_office_counts(self) -> None:
        body = ")]}'\n" + json.dumps(
            [
                {"account_id": 1, "status": "OUT_OF_OFFICE"},
                {"account_id": 2, "status": "OUTSIDE_WORKING_HOURS"},
                {"account_id": 3, "status": "NORMAL_AVAILABILITY"},
            ]
        )
        requests: list[urllib.request.Request] = []

        def urlopen(req: urllib.request.Request, timeout: int) -> Any:
            requests.append(req)
            return io.BytesIO(body.encode())

        with mock.patch.object(urllib.request, "urlopen", urlopen):
            ooo = min_owners.out_of_office(
                "https://host",
                {"a@x": 1, "b@x": 2, "c@x": 3},
                "token",
            )
        self.assertEqual(ooo, {"a@x"})
        self.assertEqual(len(requests), 1)
        self.assertEqual(
            requests[0].full_url,
            "https://host/a/plugins/availability/statuses/?id=1&id=2&id=3",
        )
        self.assertEqual(
            requests[0].get_header("Authorization"), "Bearer token"
        )

    def test_ignores_people_back_soon(self) -> None:
        now = 1_000_000
        body = ")]}'\n" + json.dumps(
            [
                {
                    "account_id": 1,
                    "status": "OUT_OF_OFFICE",
                    "return_time": {"seconds": now + 60 * 60},
                },
                {
                    "account_id": 2,
                    "status": "OUT_OF_OFFICE",
                    "return_time": {"seconds": now + 24 * 60 * 60},
                },
            ]
        )

        def urlopen(req: urllib.request.Request, timeout: int) -> Any:
            return io.BytesIO(body.encode())

        with mock.patch.object(urllib.request, "urlopen", urlopen):
            ooo = min_owners.out_of_office(
                "https://host", {"soon@x": 1, "later@x": 2}, "token", now=now
            )
        self.assertEqual(ooo, {"later@x"})

    def test_batches_requests(self) -> None:
        ids = {f"o{i}@x": i for i in range(250)}
        requests: list[str] = []

        def urlopen(req: urllib.request.Request, timeout: int) -> Any:
            requests.append(req.full_url)
            query = urllib.parse.urlparse(req.full_url).query
            statuses = [
                {"account_id": int(i), "status": "OUT_OF_OFFICE"}
                for _, i in urllib.parse.parse_qsl(query)
            ]
            return io.BytesIO((")]}'\n" + json.dumps(statuses)).encode())

        with mock.patch.object(urllib.request, "urlopen", urlopen):
            ooo = min_owners.out_of_office("https://host", ids, "token")
        self.assertEqual(len(requests), 3)
        self.assertEqual(ooo, set(ids))

    def test_failures_become_runtime_errors(self) -> None:
        def timeout(req: urllib.request.Request, timeout: int) -> Any:
            raise TimeoutError("read timed out")

        def bad_shape(req: urllib.request.Request, timeout: int) -> Any:
            return io.BytesIO(b')]}\'\n[{"account_id": 99}]')

        for urlopen in (timeout, bad_shape):
            with self.subTest(urlopen=urlopen.__name__):
                with mock.patch.object(urllib.request, "urlopen", urlopen):
                    with self.assertRaises(RuntimeError):
                        min_owners.out_of_office("https://h", {"a@x": 1}, "t")


class LuciAuthTokenTest(unittest.TestCase):
    def test_missing_binary(self) -> None:
        with (
            mock.patch.object(os.path, "exists", return_value=False),
            mock.patch.object(shutil, "which", return_value=None),
        ):
            with self.assertRaisesRegex(RuntimeError, "on PATH"):
                min_owners.luci_auth_token()

    def test_not_logged_in(self) -> None:
        proc = subprocess.CompletedProcess(
            [], 1, stdout="", stderr="Not logged in.\n"
        )
        with mock.patch.object(shutil, "which", return_value="/luci-auth"):
            with mock.patch.object(subprocess, "run", return_value=proc):
                with self.assertRaisesRegex(RuntimeError, "Not logged in"):
                    min_owners.luci_auth_token()


if __name__ == "__main__":
    unittest.main()
