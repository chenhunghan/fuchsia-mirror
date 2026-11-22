# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import csv
import io
import json
import os
import statistics
import subprocess
import tempfile
import threading
import unittest
import unittest.mock
from typing import Any, Callable, Dict, List, Optional, Tuple, Union

import local_workflow
import perfcompare

Behavior = Union[
    str,
    Callable[[List[str]], subprocess.CompletedProcess[str]],
    Exception,
]


# Test case helper class for creating temporary directories that will
# be cleaned up when the test finishes.
class TempDirTestCase(unittest.TestCase):
    def MakeTempDir(self):
        return self.enterContext(
            tempfile.TemporaryDirectory(
                prefix="tmp_unittest_%s_" % self.__class__.__name__
            )
        )


def WriteJsonFile(filename, json_data):
    with open(filename, "w") as fh:
        json.dump(json_data, fh)


class LocalWorkflowTest(TempDirTestCase):
    def setUp(self):
        super().setUp()
        self.state_file = os.path.join(self.MakeTempDir(), "state.json")
        state_data = {
            "2026-04-09": {
                "launched": {
                    "builder1": {"build_id": "1001"},
                }
            },
            "2026-04-10": {
                "launched": {
                    "builder1": {"build_id": "1002"},
                    "builder2": {"build_id": "1003"},
                }
            },
        }
        with open(self.state_file, "w") as f:
            json.dump(state_data, f)

        # Patch the code for finding the "bb" and "cas" tools so that the
        # test does not depend on the FUCHSIA_DIR env var being set and the
        # tools being present.
        self.enterContext(
            unittest.mock.patch(
                "local_workflow._find_bb_tool", lambda: "bb_for_testing"
            )
        )
        self.enterContext(
            unittest.mock.patch(
                "local_workflow._find_cas_tool", lambda: "cas_for_testing"
            )
        )

    @staticmethod
    def _make_cas_download_fake(
        content: str,
    ) -> Callable[[List[str]], subprocess.CompletedProcess[str]]:
        def handler(cmd: List[str]) -> subprocess.CompletedProcess[str]:
            dir_index = cmd.index("-dir")
            target_dir = cmd[dir_index + 1]
            os.makedirs(target_dir, exist_ok=True)
            with open(os.path.join(target_dir, "metrics.json"), "w") as f:
                f.write(content)
            return subprocess.CompletedProcess(cmd, 0, stdout="download ok")

        return handler

    @staticmethod
    def _make_bb_cas_fake(
        mapping: Dict[Tuple[str, ...], Behavior]
    ) -> Callable[[List[str]], subprocess.CompletedProcess[str]]:
        lock = threading.Lock()

        def side_effect(
            cmd: List[str], **kwargs: Any
        ) -> subprocess.CompletedProcess[str]:
            for patterns, behavior in mapping.items():
                if all(p in cmd for p in patterns):
                    if callable(behavior):
                        # The code under test runs `cas` commands from
                        # multiple threads.
                        with lock:
                            return behavior(cmd)
                    elif isinstance(behavior, Exception):
                        raise behavior
                    elif isinstance(behavior, str):
                        return subprocess.CompletedProcess(
                            cmd, 0, stdout=behavior, stderr=""
                        )
                    else:
                        raise ValueError(
                            f"Unexpected behavior type: {type(behavior)}"
                        )
            raise RuntimeError(f"No match found for command: {cmd}")

        return side_effect

    @staticmethod
    def _build_info(
        properties: Optional[Dict[str, Any]] = None,
        steps: Optional[List[Any]] = None,
    ) -> str:
        """Returns `bb get -fields output.properties,steps -json` output."""
        return json.dumps(
            {"output": {"properties": properties or {}}, "steps": steps or []}
        )

    @staticmethod
    def _task_step(instance: str, digest: str) -> Dict[str, str]:
        """Returns a build step that links to a task's CAS outputs."""
        return {
            "summaryMarkdown": (
                "* [task outputs](https://cas-viewer.appspot.com/"
                f"{instance}/blobs/{digest}/tree)"
            )
        }

    @staticmethod
    def _make_cas_tree_fake(
        trees: Dict[str, Dict[str, str]]
    ) -> Callable[[List[str]], subprocess.CompletedProcess[str]]:
        """Fakes `cas tree`, given {root digest: {path: file digest}}."""

        def handler(cmd: List[str]) -> subprocess.CompletedProcess[str]:
            digest = cmd[cmd.index("-digest") + 1]
            files = {
                path: {"digest": file_digest, "size": 1}
                for path, file_digest in trees[digest].items()
            }
            return subprocess.CompletedProcess(
                cmd, 0, stdout=json.dumps({"digest": digest, "files": files})
            )

        return handler

    @staticmethod
    def _make_cas_file_download_fake() -> (
        Callable[[List[str]], subprocess.CompletedProcess[str]]
    ):
        """Fakes `cas download -file` by writing '<instance>:<digest>'."""

        def handler(cmd: List[str]) -> subprocess.CompletedProcess[str]:
            instance = cmd[cmd.index("-cas-instance") + 1]
            digest = cmd[cmd.index("-digest") + 1]
            dest = cmd[cmd.index("-file") + 1]
            with open(dest, "w") as f:
                f.write(f"{instance}:{digest}")
            return subprocess.CompletedProcess(cmd, 0, stdout="")

        return handler

    @staticmethod
    def _read_dir_tree(root_dir: str) -> Dict[str, str]:
        """Returns {relative_path: file_contents} for all files under root_dir."""
        files = {}
        for dirpath, _, filenames in os.walk(root_dir):
            for filename in filenames:
                full_path = os.path.join(dirpath, filename)
                rel_path = os.path.relpath(full_path, root_dir)
                with open(full_path) as f:
                    files[rel_path] = f.read()
        return files

    def test_download_metrics(self):
        # Use unittest.TestCase.subTest rather than third_party/parameterized
        # so these tests remain runnable directly via python3 without
        # requiring non-stdlib packages to be installed or packaged via GN.
        for build_id in ("1000", "b1000"):
            with (
                self.subTest(build_id=build_id),
                unittest.mock.patch("subprocess.run") as mock_run,
            ):
                mock_run.side_effect = self._make_bb_cas_fake(
                    {
                        ("auth-info",): "auth ok",
                        ("get", "-fields"): self._build_info(
                            {
                                "cas_instance": "chromium_swarm",
                                "perf_dataset_digest": "123abc/82",
                            }
                        ),
                        ("download", "-dir"): self._make_cas_download_fake(
                            '{"metric": 1.0}'
                        ),
                    }
                )

                out_dir = self.MakeTempDir()
                stdout = io.StringIO()
                perfcompare.Main(
                    ["download_metrics", build_id, out_dir], stdout
                )

                self.assertEqual(
                    self._read_dir_tree(out_dir),
                    {"metrics.json": '{"metric": 1.0}'},
                )
                output = stdout.getvalue()
                self.assertIn("Fetching build info for 1000 using bb", output)
                self.assertIn("Downloading from cas to", output)
                bb_get_cmd = [
                    call[0][0]
                    for call in mock_run.call_args_list
                    if "get" in call[0][0]
                ][0]
                self.assertEqual(bb_get_cmd[-1], "1000")

    def test_normalize_build_id(self):
        self.assertEqual(local_workflow._normalize_build_id("1000"), "1000")
        self.assertEqual(local_workflow._normalize_build_id("b1000"), "1000")
        self.assertEqual(local_workflow._normalize_build_id("B1000"), "1000")
        self.assertEqual(local_workflow._normalize_build_id(" b1000 "), "1000")

    @unittest.mock.patch("subprocess.run")
    def test_download_metrics_perfcompare_digest(self, mock_run):
        mock_run.side_effect = self._make_bb_cas_fake(
            {
                ("auth-info",): "auth ok",
                ("get", "-fields"): self._build_info(
                    {
                        "cas_instance": "chromium_swarm",
                        "perfcompare_dataset_digest": "123abc/82",
                    }
                ),
                ("download", "-dir"): self._make_cas_download_fake(
                    '{"metric": 1.0}'
                ),
            }
        )

        out_dir = self.MakeTempDir()
        stdout = io.StringIO()
        perfcompare.Main(["download_metrics", "1000", out_dir], stdout)

        self.assertEqual(
            self._read_dir_tree(out_dir), {"metrics.json": '{"metric": 1.0}'}
        )
        output = stdout.getvalue()
        self.assertIn("Fetching build info for 1000 using bb", output)
        self.assertIn("Downloading from cas to", output)

    @unittest.mock.patch("subprocess.run")
    def test_download_metrics_missing_properties(self, mock_run):
        # A dataset digest without a cas_instance should fail rather than
        # guess an instance.
        mock_run.side_effect = self._make_bb_cas_fake(
            {
                ("auth-info",): "auth ok",
                ("get", "-fields"): self._build_info(
                    {"perf_dataset_digest": "123abc/82"}
                ),
            }
        )

        with self.assertRaises(RuntimeError) as cm:
            perfcompare.Main(
                ["download_metrics", "1000", self.MakeTempDir()], io.StringIO()
            )
        self.assertIn("Download failed for build 1000", str(cm.exception))
        self.assertIsInstance(cm.exception.__cause__, KeyError)
        self.assertIn("cas_instance", str(cm.exception.__cause__))

    @unittest.mock.patch("subprocess.run")
    def test_download_metrics_bb_get_fails(self, mock_run):
        mock_run.side_effect = self._make_bb_cas_fake(
            {
                ("auth-info",): "auth ok",
                ("get", "-fields"): subprocess.CalledProcessError(
                    1, ["bb", "get"], stderr="bb error"
                ),
            }
        )

        with self.assertRaises(RuntimeError) as cm:
            perfcompare.Main(
                ["download_metrics", "1000", self.MakeTempDir()], io.StringIO()
            )
        self.assertIn("Download failed for build 1000", str(cm.exception))

    @unittest.mock.patch("subprocess.run")
    def test_download_metrics_cas_download_fails(self, mock_run):
        mock_run.side_effect = self._make_bb_cas_fake(
            {
                ("auth-info",): "auth ok",
                ("get", "-fields"): self._build_info(
                    {
                        "cas_instance": "chromium_swarm",
                        "perf_dataset_digest": "123abc/82",
                    }
                ),
                ("download",): subprocess.CalledProcessError(
                    1, ["cas", "download"], stderr="cas error"
                ),
            }
        )

        with self.assertRaises(RuntimeError) as cm:
            perfcompare.Main(
                ["download_metrics", "1000", self.MakeTempDir()],
                io.StringIO(),
            )
        self.assertIn("Download failed for build 1000", str(cm.exception))

    @unittest.mock.patch("subprocess.run")
    def test_download_metrics_fallback_success(self, mock_run):
        # Set up a build with no perf_dataset_digest, a failed test in
        # output.properties, and three test task CAS trees in the build steps.
        mock_run.side_effect = self._make_bb_cas_fake(
            {
                ("auth-info",): "auth ok",
                ("get", "-fields"): self._build_info(
                    {
                        "failed_test_names": [
                            "fuchsia-pkg://fuchsia.com/failing#meta/failing.cm"
                        ]
                    },
                    [
                        {"name": "no summary"},
                        self._task_step("projects/p1/instances/i1", "aaaa/1"),
                        self._task_step("projects/p1/instances/i1", "bbbb/2"),
                        # A repeated link to the first task must not produce a
                        # second boot directory.
                        self._task_step("projects/p1/instances/i1", "aaaa/1"),
                        self._task_step("projects/p2/instances/i2", "cccc/3"),
                    ],
                ),
                ("tree",): self._make_cas_tree_fake(
                    {
                        "aaaa/1": {
                            "out/test1/0/a2.fuchsiaperf.json": "a2/10",
                            "out/test1/0/a1.fuchsiaperf.json": "a1/10",
                            "out/test1/0/trace.fxt": "a3/1000000000",
                            "out/summary.json": "a4/10",
                            # Results from the failed test are excluded.
                            # testrunner removes colons from the test name
                            # and then URL-escapes it to name the directory.
                            "out/fuchsia-pkg%2F%2Ffuchsia.com%2Ffailing%23meta%2Ffailing.cm/0/a5.fuchsiaperf.json": "a5/10",
                        },
                        # A task without perf results is skipped so that
                        # boot directories are numbered contiguously.
                        "bbbb/2": {"out/test2/0/log.txt": "b1/10"},
                        "cccc/3": {"out/test3/0/c1.fuchsiaperf.json": "c1/10"},
                    }
                ),
                ("download", "-file"): self._make_cas_file_download_fake(),
            }
        )

        out_dir = self.MakeTempDir()
        stdout = io.StringIO()
        perfcompare.Main(["download_metrics", "1000", out_dir], stdout)

        # Check that only *.fuchsiaperf.json files from passing tests were
        # downloaded, with the "out/" prefix stripped and grouped into
        # bootNNNNNN directories.
        self.assertEqual(
            self._read_dir_tree(out_dir),
            {
                os.path.join(
                    "by_boot", "boot000000", "test1", "0", "a1.fuchsiaperf.json"
                ): "projects/p1/instances/i1:a1/10",
                os.path.join(
                    "by_boot", "boot000000", "test1", "0", "a2.fuchsiaperf.json"
                ): "projects/p1/instances/i1:a2/10",
                os.path.join(
                    "by_boot", "boot000001", "test3", "0", "c1.fuchsiaperf.json"
                ): "projects/p2/instances/i2:c1/10",
            },
        )
        output = stdout.getvalue()
        self.assertIn(
            "Excluding results from failed tests:\n"
            "  fuchsia-pkg://fuchsia.com/failing#meta/failing.cm\n",
            output,
        )

    @unittest.mock.patch("subprocess.run")
    def test_download_metrics_fallback_no_task_outputs(self, mock_run):
        mock_run.side_effect = self._make_bb_cas_fake(
            {
                ("auth-info",): "auth ok",
                ("get", "-fields"): self._build_info(
                    steps=[{"summaryMarkdown": "No CAS viewer URLs here"}]
                ),
            }
        )

        with self.assertRaises(RuntimeError) as cm:
            perfcompare.Main(
                ["download_metrics", "1000", self.MakeTempDir()], io.StringIO()
            )
        self.assertIn("Download failed for build 1000", str(cm.exception))
        self.assertIn(
            "No task outputs found in build steps for 1000",
            str(cm.exception.__cause__),
        )

    @unittest.mock.patch("subprocess.run")
    def test_download_metrics_fallback_no_perf_results(self, mock_run):
        mock_run.side_effect = self._make_bb_cas_fake(
            {
                ("auth-info",): "auth ok",
                ("get", "-fields"): self._build_info(
                    steps=[self._task_step("projects/p/instances/i", "aaaa/1")]
                ),
                ("tree",): self._make_cas_tree_fake(
                    {"aaaa/1": {"out/log.txt": "a1/10"}}
                ),
            }
        )

        out_dir = self.MakeTempDir()
        with self.assertRaises(RuntimeError) as cm:
            perfcompare.Main(
                ["download_metrics", "1000", out_dir], io.StringIO()
            )
        self.assertIn(
            "No .fuchsiaperf.json files found in task outputs for build 1000",
            str(cm.exception.__cause__),
        )
        self.assertEqual(os.listdir(out_dir), [])

    @unittest.mock.patch("subprocess.run")
    def test_download_metrics_fallback_by_boot_already_exists(self, mock_run):
        mock_run.side_effect = self._make_bb_cas_fake(
            {
                ("auth-info",): "auth ok",
                ("get", "-fields"): self._build_info(
                    steps=[self._task_step("projects/p/instances/i", "aaaa/1")]
                ),
            }
        )

        out_dir = self.MakeTempDir()
        old_file = os.path.join(out_dir, "by_boot", "boot000000", "old.txt")
        os.makedirs(os.path.dirname(old_file))
        with open(old_file, "w") as f:
            f.write("old data")

        with self.assertRaises(RuntimeError) as cm:
            perfcompare.Main(
                ["download_metrics", "1000", out_dir], io.StringIO()
            )
        self.assertIn("already exists", str(cm.exception.__cause__))
        # Check that the existing directory was left untouched.
        self.assertEqual(
            self._read_dir_tree(out_dir),
            {os.path.join("by_boot", "boot000000", "old.txt"): "old data"},
        )

    @unittest.mock.patch("subprocess.run")
    def test_download_metrics_fallback_cas_download_fails(self, mock_run):
        def failing_download(
            cmd: List[str],
        ) -> subprocess.CompletedProcess[str]:
            dest = cmd[cmd.index("-file") + 1]
            if cmd[cmd.index("-digest") + 1] == "a2/10":
                raise subprocess.CalledProcessError(1, cmd)
            with open(dest, "w") as f:
                f.write("partial")
            return subprocess.CompletedProcess(cmd, 0, stdout="")

        mock_run.side_effect = self._make_bb_cas_fake(
            {
                ("auth-info",): "auth ok",
                ("get", "-fields"): self._build_info(
                    steps=[self._task_step("projects/p/instances/i", "aaaa/1")]
                ),
                ("tree",): self._make_cas_tree_fake(
                    {
                        "aaaa/1": {
                            "out/a1.fuchsiaperf.json": "a1/10",
                            "out/a2.fuchsiaperf.json": "a2/10",
                        }
                    }
                ),
                ("download", "-file"): failing_download,
            }
        )

        out_dir = self.MakeTempDir()
        with self.assertRaises(RuntimeError) as cm:
            perfcompare.Main(
                ["download_metrics", "1000", out_dir], io.StringIO()
            )
        self.assertIn("Download failed for build 1000", str(cm.exception))
        # Check that neither a partial by_boot directory nor the staging
        # directory is left behind.
        self.assertEqual(os.listdir(out_dir), [])

    def test_extract_task_outputs_from_steps(self):
        steps = [
            {"summaryMarkdown": "No url here"},
            {"otherField": "No summaryMarkdown"},
            {
                "summaryMarkdown": (
                    "* [task outputs](https://cas-viewer.appspot.com/projects/p2/instances/i2/blobs/5678/20/tree)\n"
                    "* [task outputs](https://cas-viewer.api.luci.app/projects/p1/instances/i1/blobs/ABCD12/10/tree)"
                )
            },
            {
                # The same task, linked from a different host.
                "summaryMarkdown": (
                    "* [task outputs](https://cas-viewer.api.luci.app/projects/p1/instances/i1/blobs/ABCD12/10/tree)"
                )
            },
            {
                # The same digest in a different instance is kept.
                "summaryMarkdown": (
                    "* [task outputs](https://cas-viewer.appspot.com/projects/p2/instances/i2/blobs/ABCD12/10/tree)"
                )
            },
            {
                # Links to other CAS uploads, such as the aggregated perf
                # dataset, are ignored.
                "summaryMarkdown": (
                    "* [CAS UI](https://cas-viewer.appspot.com/projects/p3/instances/i3/blobs/EEEE/82/tree)\n"
                    "* [task cas outputs: shard](https://cas-viewer.appspot.com/projects/p3/instances/i3/blobs/FFFF/10/tree)"
                )
            },
        ]
        self.assertEqual(
            local_workflow._extract_task_outputs_from_steps(steps),
            [
                ("projects/p1/instances/i1", "ABCD12/10"),
                ("projects/p2/instances/i2", "5678/20"),
                ("projects/p2/instances/i2", "ABCD12/10"),
            ],
        )
        self.assertEqual(
            local_workflow._extract_task_outputs_from_steps([]), []
        )

    def test_parallel_map(self):
        self.assertEqual(
            local_workflow._parallel_map(lambda x: x * x, range(20)),
            [x * x for x in range(20)],
        )

        def fail_on_3(x: int) -> int:
            if x == 3:
                raise ValueError("boom")
            return x

        with self.assertRaisesRegex(ValueError, "boom"):
            local_workflow._parallel_map(fail_on_3, range(10))

    @unittest.mock.patch("subprocess.run")
    def test_download_baseline_success(self, mock_run):
        mock_run.side_effect = self._make_bb_cas_fake(
            {
                ("auth-info",): "auth ok",
                # Order matters: check for specific 'get' with '-fields'
                # before general 'get'.
                ("get", "-fields"): self._build_info(
                    {
                        "cas_instance": "chromium_swarm",
                        "perf_dataset_digest": "123abc/82",
                    }
                ),
                ("get",): json.dumps({"status": "SUCCESS"}),
                ("download",): self._make_cas_download_fake('{"metric": 1.0}'),
            }
        )

        out_dir = self.MakeTempDir()
        stdout = io.StringIO()

        perfcompare.Main(
            [
                "download_baseline",
                "--state_file",
                self.state_file,
                "builder2",
                out_dir,
            ],
            stdout,
        )

        expected_file = os.path.join(out_dir, "metrics.json")
        self.assertTrue(os.path.exists(expected_file))
        with open(expected_file) as f:
            self.assertEqual(f.read(), '{"metric": 1.0}')

        output = stdout.getvalue()
        self.assertIn("1003 (SUCCESS) - Triggering download", output)

    @unittest.mock.patch("subprocess.run")
    def test_download_baseline_fallback(self, mock_run):
        mock_run.side_effect = self._make_bb_cas_fake(
            {
                ("auth-info",): "auth ok",
                ("get", "1002"): json.dumps({"status": "FAILURE"}),
                # Order matters: check for specific 'get' with '-fields'
                # before general 'get'.
                ("get", "1001", "-fields"): self._build_info(
                    {
                        "cas_instance": "chromium_swarm",
                        "perf_dataset_digest": "123abc/82",
                    }
                ),
                ("get", "1001"): json.dumps({"status": "SUCCESS"}),
                ("download",): self._make_cas_download_fake('{"metric": 1.0}'),
            }
        )

        out_dir = self.MakeTempDir()
        stdout = io.StringIO()

        perfcompare.Main(
            [
                "download_baseline",
                "--state_file",
                self.state_file,
                "builder1",
                out_dir,
            ],
            stdout,
        )

        expected_file = os.path.join(out_dir, "metrics.json")
        self.assertTrue(os.path.exists(expected_file))
        with open(expected_file) as f:
            self.assertEqual(f.read(), '{"metric": 1.0}')

        output = stdout.getvalue()
        self.assertIn("1002 (FAILURE) - Skipping", output)
        self.assertIn("1001 (SUCCESS) - Triggering download", output)

    @unittest.mock.patch("subprocess.run")
    def test_download_baseline_no_successful_build(self, mock_run):
        mock_run.side_effect = self._make_bb_cas_fake(
            {
                ("auth-info",): "auth ok",
                ("get",): json.dumps({"status": "FAILURE"}),
            }
        )

        out_dir = self.MakeTempDir()
        stdout = io.StringIO()

        perfcompare.Main(
            [
                "download_baseline",
                "--state_file",
                self.state_file,
                "builder1",
                out_dir,
            ],
            stdout,
        )

        output = stdout.getvalue()
        self.assertIn("1002 (FAILURE) - Skipping", output)
        self.assertIn("1001 (FAILURE) - Skipping", output)
        self.assertIn("No successful sample build IDs found", output)

    def test_extract_metrics(self):
        data_dir = self.MakeTempDir()
        WriteJsonFile(
            os.path.join(data_dir, "test.fuchsiaperf.json"),
            [
                {
                    "test_suite": "fuchsia.test",
                    "label": "metric1",
                    "values": [10.5],
                },
                {
                    "test_suite": "fuchsia.test",
                    "label": "metric2",
                    "values": [20.0],
                },
                {
                    "test_suite": "other.suite",
                    "label": "metric3",
                    "values": [30.0],
                },
            ],
        )

        stdout = io.StringIO()
        perfcompare.Main(["extract_metrics", "fuchsia", data_dir], stdout)

        output = stdout.getvalue()
        self.assertIn("fuchsia.test,metric1,10.5", output)
        self.assertIn("fuchsia.test,metric2,20.0", output)
        self.assertNotIn("other.suite", output)

    def test_extract_metrics_aggregation(self):
        data_dir = self.MakeTempDir()
        WriteJsonFile(
            os.path.join(data_dir, "test1.fuchsiaperf.json"),
            [
                {
                    "test_suite": "fuchsia.test",
                    "label": "metric1",
                    "values": [10.5],
                }
            ],
        )
        WriteJsonFile(
            os.path.join(data_dir, "test2.fuchsiaperf.json"),
            [
                {
                    "test_suite": "fuchsia.test",
                    "label": "metric1",
                    "values": [15.0],
                }
            ],
        )

        stdout = io.StringIO()
        perfcompare.Main(["extract_metrics", "fuchsia", data_dir], stdout)

        stdout.seek(0)
        reader = csv.reader(stdout)
        rows = list(reader)
        self.assertEqual(rows, [["fuchsia.test", "metric1", "10.5", "15.0"]])

    def test_extract_metrics_empty_values(self):
        data_dir = self.MakeTempDir()
        WriteJsonFile(
            os.path.join(data_dir, "test.fuchsiaperf.json"),
            [
                {
                    "test_suite": "fuchsia.test",
                    "label": "metric1",
                    "values": [],
                }
            ],
        )

        with self.assertRaises(statistics.StatisticsError):
            perfcompare.Main(
                ["extract_metrics", "fuchsia", data_dir], io.StringIO()
            )

    def test_extract_metrics_malformed_json(self):
        data_dir = self.MakeTempDir()
        file_path = os.path.join(data_dir, "bad.fuchsiaperf.json")
        with open(file_path, "w") as f:
            f.write("{invalid}")

        with self.assertRaises(RuntimeError) as cm:
            perfcompare.Main(
                ["extract_metrics", "fuchsia", data_dir], io.StringIO()
            )
        self.assertIn("Malformed JSON", str(cm.exception))


if __name__ == "__main__":
    unittest.main()
