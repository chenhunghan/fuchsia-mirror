# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for evidence recording."""

import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from driver_lab.evidence import EvidenceError, EvidenceRecorder


class EvidenceRecorderTest(unittest.TestCase):
    def setUp(self) -> None:
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        self.root = Path(self._dir.name)

    def test_directory_is_never_reused(self) -> None:
        EvidenceRecorder(self.root, "run-1")
        with self.assertRaises(EvidenceError):
            EvidenceRecorder(self.root, "run-1")

    def test_invalid_run_ids_rejected(self) -> None:
        for run_id in ("../evil", "a/b", "a..b", ""):
            with self.assertRaises(EvidenceError):
                EvidenceRecorder(self.root, run_id)

    def test_artifacts_are_hashed_and_atomic(self) -> None:
        recorder = EvidenceRecorder(self.root, "run-1")
        recorder.write_json("plan.canonical.json", {"a": 1})
        manifest_path = recorder.finalize(0)
        manifest = json.loads(manifest_path.read_text())
        entry = manifest["files"]["plan.canonical.json"]
        data = (recorder.directory / "plan.canonical.json").read_bytes()
        self.assertEqual(entry["sha256"], hashlib.sha256(data).hexdigest())
        self.assertEqual(entry["bytes"], len(data))
        # No temporary files remain.
        self.assertEqual(
            sorted(path.name for path in recorder.directory.iterdir()),
            ["manifest.json", "plan.canonical.json"],
        )

    def test_manifest_is_written_last(self) -> None:
        recorder = EvidenceRecorder(self.root, "run-1")
        recorder.write_json("a.json", {})
        self.assertFalse((recorder.directory / "manifest.json").exists())
        recorder.finalize(0)
        self.assertTrue((recorder.directory / "manifest.json").exists())

    def test_finalized_recorder_is_sealed(self) -> None:
        recorder = EvidenceRecorder(self.root, "run-1")
        recorder.finalize(0)
        with self.assertRaises(EvidenceError):
            recorder.write_json("late.json", {})
        with self.assertRaises(EvidenceError):
            recorder.finalize(0)

    def test_duplicate_artifact_rejected(self) -> None:
        recorder = EvidenceRecorder(self.root, "run-1")
        recorder.write_json("a.json", {})
        with self.assertRaises(EvidenceError):
            recorder.write_json("a.json", {})

    def test_manifest_cannot_be_written_directly(self) -> None:
        recorder = EvidenceRecorder(self.root, "run-1")
        with self.assertRaises(EvidenceError):
            recorder.write_json("manifest.json", {})

    def test_not_applicable_marker(self) -> None:
        recorder = EvidenceRecorder(self.root, "run-1")
        recorder.mark_not_applicable("restoration.json")
        manifest = json.loads(recorder.finalize(0).read_text())
        self.assertEqual(
            manifest["files"]["restoration.json"], "not_applicable"
        )
        self.assertFalse((recorder.directory / "restoration.json").exists())

    def test_jsonl_round_trip(self) -> None:
        recorder = EvidenceRecorder(self.root, "run-1")
        rows = [{"seq": 1}, {"seq": 2}]
        recorder.write_jsonl("ops.jsonl", rows)
        lines = (recorder.directory / "ops.jsonl").read_text().splitlines()
        self.assertEqual([json.loads(line) for line in lines], rows)

    def test_manifest_extras_recorded_and_collisions_rejected(self) -> None:
        recorder = EvidenceRecorder(self.root, "run-1")
        manifest = json.loads(
            recorder.finalize(4, {"plan_digest": "sha256:aa"}).read_text()
        )
        self.assertEqual(manifest["plan_digest"], "sha256:aa")
        self.assertEqual(manifest["exit_category"], 4)

        other = EvidenceRecorder(self.root, "run-2")
        with self.assertRaises(EvidenceError):
            other.finalize(0, {"files": {}})


if __name__ == "__main__":
    unittest.main()
