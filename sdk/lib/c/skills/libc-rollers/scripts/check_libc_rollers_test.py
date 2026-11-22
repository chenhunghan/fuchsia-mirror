# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for check_libc_rollers and the libc_rollers package."""

import io
import json
import os
import pathlib
import stat
import string
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import check_libc_rollers
from libc_rollers import (
    config,
    constants,
    formatting,
    gerrit,
    luci,
    metrics,
    monitor,
    triage,
    utils,
)


def _write_executable(path: pathlib.Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


class CheckLibcRollersTest(unittest.TestCase):
    def setUp(self) -> None:
        utils.clear_caches_and_overrides()

    def tearDown(self) -> None:
        utils.clear_caches_and_overrides()

    def test_resolve_tool_bin_precedence_and_missing_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            fake_bb = root / "custom-bb"
            _write_executable(fake_bb, "#!/bin/sh\nexit 0\n")

            utils.set_tool_overrides(bb_bin=fake_bb)
            self.assertEqual(
                utils.resolve_tool_bin(root, "bb"),
                fake_bb.resolve(),
            )

            utils.set_tool_overrides(bb_bin=None)
            with mock.patch.dict(
                os.environ, {"BB_BIN": str(fake_bb)}, clear=False
            ):
                self.assertEqual(
                    utils.resolve_tool_bin(root, "bb"),
                    fake_bb.resolve(),
                )

            prebuilt_bb = root / "prebuilt" / "tools" / "buildbucket" / "bb"
            _write_executable(prebuilt_bb, "#!/bin/sh\nexit 0\n")
            with (
                mock.patch.object(
                    pathlib.Path, "home", autospec=True, return_value=root
                ),
                mock.patch.dict(
                    os.environ, {"BB_BIN": "", "PATH": ""}, clear=False
                ),
            ):
                self.assertEqual(
                    utils.resolve_tool_bin(root, "bb"), prebuilt_bb
                )
                self.assertIsNone(utils.resolve_tool_bin(root, "luci-auth"))
                with self.assertRaises(FileNotFoundError):
                    utils.require_tool_bin(root, "luci-auth")

    def test_decode_json_stream_reader_and_atomic_write(self) -> None:
        raw_stream = (
            '{"id": "1", "status": "SUCCESS"}\n'
            '  {"id": "2",\n   "status": "FAILURE"}  \n'
        )
        parsed = utils.decode_json_stream_reader(
            io.StringIO(raw_stream), chunk_size=7
        )
        self.assertSequenceEqual(
            parsed,
            (
                {"id": "1", "status": "SUCCESS"},
                {"id": "2", "status": "FAILURE"},
            ),
        )

        with tempfile.TemporaryDirectory() as tmp:
            out_path = pathlib.Path(tmp) / "nested" / "state.json"
            utils.atomic_write_json(out_path, {"ok": True, "count": 3})
            self.assertEqual(
                json.loads(out_path.read_text(encoding="utf-8")),
                {"ok": True, "count": 3},
            )

    def test_get_luci_token_falls_back_to_credential_helper_json(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            failing_luci_auth = root / "luci-auth"
            _write_executable(failing_luci_auth, "#!/bin/sh\nexit 1\n")
            helper = root / "cred_helper.sh"
            _write_executable(
                helper,
                '#!/bin/sh\necho \'{"token":"tok-abc-123","expiry":1999999999}\'\n',
            )
            utils.set_tool_overrides(luci_auth_bin=failing_luci_auth)
            with mock.patch.dict(
                os.environ,
                {"LUCI_AUTH_CREDENTIAL_HELPER": str(helper)},
                clear=False,
            ):
                tok = luci.get_luci_token(root)
            self.assertEqual(tok, "tok-abc-123")

    def test_inspect_roll_build_steps_streams_step_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            fake_bb = root / "bb"
            _write_executable(
                fake_bb,
                "#!/bin/sh\n"
                "echo 'Step \"restore CL from previous roll attempt\"'\n"
                "echo 'Step \"get gerrit change numbers for llvm|4 commits\"'\n"
                "echo 'Step \"roll 1234567890abcdef\"'\n"
                'echo "https://ci.chromium.org?change%22%3A+%221987654%22%2C+%22patchset%22%3A+%223%22"\n',
            )
            utils.set_tool_overrides(bb_bin=fake_bb)
            step_info = luci.inspect_roll_build_steps(root, "8700000099")
            self.assertEqual(
                step_info,
                {
                    "cl_num": "1987654",
                    "patchset": "3",
                    "commit_count": 4,
                    "roll_revision": "1234567890abcdef",
                    "restored_previous_attempt": True,
                },
            )

            _write_executable(
                fake_bb,
                '#!/bin/sh\necho \'{"id": "8700000100", "status": "SUCCESS"}\'\n',
            )
            cl_url, cl_builds = luci.fetch_cl_builds(root, "1987654", "3")
            self.assertEqual(
                cl_url,
                "https://fuchsia-review.googlesource.com/c/fuchsia/+/1987654/3",
            )
            self.assertSequenceEqual(
                cl_builds, ({"id": "8700000100", "status": "SUCCESS"},)
            )

            _write_executable(fake_bb, "#!/bin/sh\nexit 2\n")
            with self.assertRaises(subprocess.CalledProcessError):
                luci.fetch_cl_builds(root, "1987654", "3")

    @mock.patch.object(utils, "fetch_remote_gob_text", autospec=True)
    @mock.patch.object(luci, "get_luci_token", autospec=True)
    @mock.patch.object(luci, "scheduler_rpc", autospec=True)
    @mock.patch.dict(os.environ, {"LUCI_PROJECT": ""}, clear=False)
    def test_checkout_free_gitiles_and_live_scheduler_resolution(
        self,
        mock_rpc: mock.MagicMock,
        mock_token: mock.MagicMock,
        mock_fetch: mock.MagicMock,
    ) -> None:
        fake_manual_roll = (
            "declare -r -A REPOS=(\n"
            "  [third_party/llvm-libc/src]=llvm-project/libc\n"
            "  [third_party/scudo/src]=scudo\n"
            ")\n"
        )
        mock_fetch.side_effect = (
            "[LUCI]: https://luci-milo.appspot.com/ui/p/customproj/g/global_roller/builders\n",
            fake_manual_roll,
        )
        mock_token.return_value = "tok-xyz"
        mock_rpc.return_value = {
            "jobs": [
                {"jobRef": {"job": "global.roller-scudo-roller-deadbeef"}},
                {"jobRef": {"job": "other.roller-llvm-libc-roller-cafebabe"}},
            ]
        }

        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            proj = config.parse_default_luci_project(root)
            repos = config.parse_manual_roll_repos(root)
            rollers = {
                "scudo-roller": {
                    "luci_project": proj,
                    "bucket": "global.roller",
                    "builder": f"{proj}/global.roller/scudo-roller",
                    "scheduler_job": "global.roller-scudo-roller",
                },
                "llvm-libc-roller": {
                    "luci_project": proj,
                    "bucket": "global.roller",
                    "builder": f"{proj}/global.roller/llvm-libc-roller",
                    "scheduler_job": "global.roller-llvm-libc-roller",
                },
            }
            config.resolve_live_scheduler_jobs(root, rollers)

        self.assertEqual(proj, "customproj")
        self.assertEqual(
            repos,
            {
                "third_party/llvm-libc/src": "llvm-project/libc",
                "third_party/scudo/src": "scudo",
            },
        )
        self.assertEqual(
            rollers["scudo-roller"]["scheduler_job"],
            "global.roller-scudo-roller-deadbeef",
        )
        self.assertEqual(rollers["llvm-libc-roller"]["bucket"], "other.roller")
        self.assertEqual(
            rollers["llvm-libc-roller"]["scheduler_job"],
            "other.roller-llvm-libc-roller-cafebabe",
        )

    def test_failure_patterns_and_sibling_false_rejection_propagation(
        self,
    ) -> None:
        for real_target in (
            "1 test failed: boot-libc-unittests",
            "1 test failed: libc-sanitizer-tests",
            "1 test failed: core-tests-standalone",
            "1 test failed: core-tests-hpet",
            "FAILED: obj/sdk/lib/c/dlfcn/dl/libdl.a",
        ):
            self.assertRegex(real_target, constants.REAL_FAILURE_PATTERNS)

        for unrelated_target in (
            "1 test failed: starnix_core_tests",
            "1 test failed: fidl_tests",
            "1 test failed: build-tests",
        ):
            self.assertNotRegex(
                unrelated_target, constants.REAL_FAILURE_PATTERNS
            )

        kws = triage.extract_failure_keywords(
            (
                "test:core-integration-tests",
                "target:main_test",
                "test:pkg-resolver-integration-tests",
            )
        )
        self.assertNotIn("core", kws)
        self.assertNotIn("main", kws)
        self.assertNotIn("test", kws)
        self.assertIn("pkg-resolver", kws)

        classified = triage.classify_roll_failures(
            "llvm-libc-roller",
            {"id": "8700000100", "status": "FAILURE"},
            {"cl_num": "1999000", "roll_revision": "abcdef123456"},
            [
                {
                    "id": "8700000101",
                    "status": "FAILURE",
                    "builder": {"builder": "core.x64"},
                    "summaryMarkdown": (
                        "This failure may be a false rejection.\n"
                        "FAILED: obj/src/sys/pkg/libpkg.a\n"
                    ),
                },
                {
                    "id": "8700000102",
                    "status": "FAILURE",
                    "builder": {"builder": "core.arm64"},
                    "summaryMarkdown": "FAILED: obj/src/sys/pkg/libpkg.a\n",
                },
            ],
            fuchsia_root=None,
        )
        self.assertSequenceEqual(classified["actionable"], ())
        self.assertEqual(len(classified["flakes"]), 2)
        self.assertIn(
            "[sibling false rejection]", classified["flakes"][1]["first_line"]
        )

        # Verify `-subbuild-swarming` parent wrapper deduplication in triage.
        _, brief = triage.run_triage_cl(
            pathlib.Path("/nonexistent"),
            "1981874",
            "2",
            builds=[
                {
                    "id": "8665132120941588577",
                    "status": "FAILURE",
                    "builder": {"builder": "bringup.arm64-debug"},
                    "summaryMarkdown": "[build](https://ci.chromium.org/b/1) failed:",
                },
                {
                    "id": "8665132120941588578",
                    "status": "FAILURE",
                    "builder": {
                        "builder": "bringup.arm64-debug-subbuild-swarming"
                    },
                    "summaryMarkdown": (
                        "FAILED: [code=1] "
                        "gen/sdk/lib/c/include/check-llvm-libc-generated.json.verified"
                    ),
                },
            ],
        )
        self.assertNotIn("bringup.arm64-debug ([build]", brief)
        self.assertIn(
            "bringup.arm64-debug (FAILED: [code=1] "
            "gen/sdk/lib/c/include/check-llvm-libc-generated.json.verified)",
            brief,
        )

    @mock.patch.object(gerrit, "fetch_recent_merged_rolls", autospec=True)
    def test_slo_overrides_and_tier_evaluation(
        self, mock_merged: mock.MagicMock
    ) -> None:
        mock_merged.return_value = [
            {
                "cl_num": str(1900000 + i),
                "commit": f"{i:012x}",
                "subject": "[roll] Roll scudo",
                "created_epoch": 1700000000.0,
                "submitted_epoch": 1700010800.0,
                "convergence_hours": 3.0,
                "attempts": 1,
            }
            for i in range(6)
        ]
        with tempfile.TemporaryDirectory() as tmp:
            history_file = pathlib.Path(tmp) / "history.json"
            with self.assertRaises(ValueError):
                metrics.apply_slo_overrides(
                    history_file, ("scudo-roller=30:10",)
                )
            updated = metrics.apply_slo_overrides(
                history_file, ("scudo-roller=16:32",)
            )
            self.assertEqual(updated["scudo-roller"]["target_hours"], 16.0)
            self.assertEqual(updated["scudo-roller"]["breach_hours"], 32.0)

            history = metrics.load_health_history(history_file)
            slo_warning = metrics.update_and_evaluate_roller_slo(
                history=history,
                roller_name="scudo-roller",
                manifest_project="scudo",
                schedule="0 5 * * *",
                p_info={
                    "pending_count": 2,
                    "oldest_pending_epoch": 1700000000.0,
                },
                last_landed={"cl_num": "1900005"},
                recent_builds=[{"id": "871", "status": "FAILURE"}],
                failure_classification={"actionable": []},
                pending_attempt=None,
                now_epoch=1700000000.0 + 20.0 * 3600.0,
            )
            self.assertEqual(slo_warning["status"], "warning")
            self.assertEqual(slo_warning["stats"]["sample_rolls"], 6)

    def test_sync_skill_files_round_trip(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            wt = pathlib.Path(tmp)
            skill_dir = wt / "sdk" / "lib" / "c" / "skills" / "libc-rollers"
            constants_file = (
                skill_dir / "scripts" / "libc_rollers" / "constants.py"
            )
            skill_md = skill_dir / "SKILL.md"
            constants_file.parent.mkdir(parents=True)

            skill_md.write_text(
                "# Title\n\n"
                + formatting.render_skill_table(constants.DEFAULT_ROLLERS)
                + "\n",
                encoding="utf-8",
            )
            constants_file.write_text(
                "# Header\n"
                + formatting.render_default_rollers_block(
                    constants.DEFAULT_ROLLERS
                )
                + "\n",
                encoding="utf-8",
            )

            self.assertSequenceEqual(
                formatting.sync_skill_files(
                    constants.DEFAULT_ROLLERS,
                    force_write=False,
                    worktree_dir=wt,
                ),
                (),
            )

            drifted = {k: dict(v) for k, v in constants.DEFAULT_ROLLERS.items()}
            drifted["scudo-roller"]["schedule"] = "0 6 * * *"
            drift_msgs = formatting.sync_skill_files(
                drifted, force_write=False, worktree_dir=wt
            )
            self.assertEqual(len(drift_msgs), 2)

            write_msgs = formatting.sync_skill_files(
                drifted, force_write=True, worktree_dir=wt
            )
            self.assertEqual(len(write_msgs), 2)
            self.assertIn("`0 6 * * *`", skill_md.read_text(encoding="utf-8"))
            self.assertIn(
                '"schedule": "0 6 * * *"',
                constants_file.read_text(encoding="utf-8"),
            )

    def test_run_monitor_actions_pure_json_and_preserves_caller_state_keys(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            state_file = root / "monitor_state.json"
            state_file.write_text(
                json.dumps(
                    {
                        "caller_custom_key": {"issue-1": "handled"},
                        "retriggered_builds": {},
                        "flake_streaks": {},
                    }
                ),
                encoding="utf-8",
            )

            rollers = {
                "scudo-roller": {
                    "luci_project": "fuchsia",
                    "bucket": "global.roller",
                    "scheduler_job": "global.roller-scudo-roller-1234abcd",
                    "builder": "fuchsia/global.roller/scudo-roller",
                },
                "llvm-libc-roller": {
                    "luci_project": "fuchsia",
                    "bucket": "global.roller",
                    "scheduler_job": "global.roller-llvm-libc-roller-5678ef01",
                    "builder": "fuchsia/global.roller/llvm-libc-roller",
                },
            }
            report = {
                "scudo-roller": {
                    "luci_project": "fuchsia",
                    "bucket": "global.roller",
                    "scheduler_job": "global.roller-scudo-roller-1234abcd",
                    "recent_builds": [
                        {"id": "8700000001", "status": "FAILURE"}
                    ],
                    "failure_classification": {
                        "actionable": [],
                        "flakes": [
                            {
                                "builder": "core.x64",
                                "build_id": "8700000002",
                                "first_line": "Fastboot_error",
                            }
                        ],
                        "flake_signatures": ["step:Fastboot_error"],
                    },
                    "retrigger_policy": {
                        "should_retrigger": True,
                        "signatures": ["step:Fastboot_error"],
                        "same_roller_streak": 1,
                        "streak_count": 1,
                        "is_persistent": False,
                    },
                },
                "llvm-libc-roller": {
                    "luci_project": "fuchsia",
                    "bucket": "global.roller",
                    "scheduler_job": "global.roller-llvm-libc-roller-5678ef01",
                    "remote": "https://llvm.googlesource.com/llvm-project/libc",
                    "path": "third_party/llvm-libc/src",
                    "recent_builds": [
                        {"id": "8700000010", "status": "FAILURE"}
                    ],
                    "active_or_failed_cl": (
                        "https://fuchsia-review.googlesource.com/"
                        "c/fuchsia/+/1999999/1"
                    ),
                    "pending": {
                        "pinned_revision": "111111111111",
                        "upstream_head": "222222222222",
                        "pending_count": 23,
                        "pending_commits": [
                            {"short_sha": f"{i:012x}"} for i in range(23)
                        ],
                    },
                    "failure_classification": {
                        "actionable": [
                            {
                                "builder": "core.arm64-asan",
                                "build_id": "8700000011",
                                "first_line": "libc-unittests failed",
                            }
                        ],
                        "flakes": [],
                        "issue_key": "llvm-libc-roller:1999999:222222222222",
                        "issue_title": (
                            "[llvm-libc-roller] fxr/1999999: core.arm64-asan"
                        ),
                    },
                },
            }
            log_buf = io.StringIO()
            state = monitor.load_monitor_state(state_file)
            summary = monitor.run_monitor_actions(
                root,
                rollers,
                report,
                state,
                state_file,
                disable_retrigger=True,
                log_stream=log_buf,
            )
            self.assertSequenceEqual(summary["retriggered"], ())
            self.assertEqual(len(summary["held_flakes"]), 1)
            self.assertEqual(len(summary["actionable_issues"]), 1)
            issue = summary["actionable_issues"][0]
            self.assertEqual(issue["roller"], "llvm-libc-roller")
            self.assertEqual(
                issue["builder"], "fuchsia/global.roller/llvm-libc-roller"
            )
            self.assertEqual(issue["pending_count"], 23)
            self.assertEqual(len(issue["pending_shas"]), 15)
            self.assertEqual(issue["roll_cl_num"], "1999999")
            self.assertEqual(len(issue["failing_builders"]), 1)
            self.assertTrue(
                all(c in string.printable for c in log_buf.getvalue())
            )

            persisted = json.loads(state_file.read_text(encoding="utf-8"))
            self.assertEqual(
                persisted["caller_custom_key"], {"issue-1": "handled"}
            )

    def test_cli_set_slo_and_invalid_arg_returns_nonzero_without_traceback(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            hist_path = pathlib.Path(tmp) / "history.json"
            stderr_buf = io.StringIO()
            with mock.patch.object(sys, "stderr", stderr_buf):
                rc_bad = check_libc_rollers.main(
                    ["--history-file", str(hist_path), "--set-slo", "bad-spec"]
                )
            self.assertEqual(rc_bad, 1)
            self.assertIn("error:", stderr_buf.getvalue())

            stdout_buf = io.StringIO()
            with mock.patch.object(sys, "stdout", stdout_buf):
                rc_ok = check_libc_rollers.main(
                    [
                        "--history-file",
                        str(hist_path),
                        "--set-slo",
                        "llvm-libc-roller=10:20",
                    ]
                )
            self.assertEqual(rc_ok, 0)
            self.assertIn("target <= 10h", stdout_buf.getvalue())


if __name__ == "__main__":
    unittest.main()
