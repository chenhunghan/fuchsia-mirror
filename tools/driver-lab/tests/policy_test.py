# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for target ceiling policy manifests and verification."""

import unittest

from driver_lab.policy import (
    NarrowingError,
    PolicyError,
    ResourcePolicyManifest,
    TargetPolicyManifest,
    WritableRegister,
    canonicalize_ranges,
)


class PolicyManifestTest(unittest.TestCase):
    def test_canonicalize_ranges_merges_overlapping_and_adjacent(self) -> None:
        ranges = [[10, 20], [15, 25], [25, 30], [40, 50]]
        merged = canonicalize_ranges(ranges)
        self.assertEqual(merged, [[10, 30], [40, 50]])

    def test_canonicalize_ranges_accepts_dict_input(self) -> None:
        ranges = [{"start": 10, "end": 20}, {"start": 20, "end": 30}]
        merged = canonicalize_ranges(ranges)
        self.assertEqual(merged, [[10, 30]])

    def test_canonicalize_ranges_rejects_inverted_or_empty(self) -> None:
        with self.assertRaises(PolicyError):
            canonicalize_ranges([[20, 10]])
        with self.assertRaises(PolicyError):
            canonicalize_ranges([[10, 10]])

    def test_canonical_json_and_digest_determinism(self) -> None:
        manifest = TargetPolicyManifest(
            schema_version=1,
            allow_mutating_sessions=False,
            audit_capacity=1024,
            max_snapshot_items=64,
            resources=[
                ResourcePolicyManifest(
                    id=0,
                    name="mmio0",
                    allow_unknown_reads=True,
                    allow_poll=False,
                    hard_denied=[[64, 68]],
                )
            ],
        )

        expected_json = b'{"allow_mutating_sessions":false,"audit_capacity":1024,"max_snapshot_items":64,"resources":[{"allow_poll":false,"allow_unknown_reads":true,"hard_denied":[[64,68]],"id":0,"name":"mmio0"}],"schema_version":1}'
        self.assertEqual(manifest.canonical_json(), expected_json)

        expected_digest = "sha256:8cf98e1811195e15a4db3ee2f71874f6d6b77f900e5c66938b5a532883e12fbd"
        self.assertEqual(manifest.policy_digest(), expected_digest)

    def test_engineering_default(self) -> None:
        default = TargetPolicyManifest.engineering_default(
            {0: "mmio0", 1: "mmio1"}
        )
        self.assertEqual(len(default.resources), 2)
        self.assertEqual(default.resources[0].id, 0)
        self.assertEqual(default.resources[0].name, "mmio0")
        self.assertTrue(default.resources[0].allow_unknown_reads)
        self.assertFalse(default.resources[0].allow_poll)
        self.assertEqual(default.resources[0].hard_denied, [])
        self.assertFalse(default.allow_mutating_sessions)

    def test_narrowing_accepts_valid_reductions(self) -> None:
        baseline = TargetPolicyManifest(
            schema_version=1,
            allow_mutating_sessions=True,
            audit_capacity=1024,
            max_snapshot_items=64,
            resources=[
                ResourcePolicyManifest(
                    id=0,
                    name="mmio0",
                    allow_unknown_reads=True,
                    allow_poll=True,
                    hard_denied=[[64, 68]],
                )
            ],
        )

        runtime = TargetPolicyManifest(
            schema_version=1,
            allow_mutating_sessions=False,
            audit_capacity=512,
            max_snapshot_items=32,
            resources=[
                ResourcePolicyManifest(
                    id=0,
                    name="mmio0",
                    allow_unknown_reads=False,
                    allow_poll=False,
                    hard_denied=[[64, 68], [128, 144]],
                )
            ],
        )

        narrowed = baseline.narrow_with(runtime)
        self.assertEqual(narrowed.audit_capacity, 512)
        self.assertEqual(narrowed.max_snapshot_items, 32)
        self.assertFalse(narrowed.allow_mutating_sessions)
        self.assertFalse(narrowed.resources[0].allow_unknown_reads)
        self.assertFalse(narrowed.resources[0].allow_poll)
        self.assertEqual(len(narrowed.resources[0].hard_denied), 2)

    def test_narrowing_rejects_widenings(self) -> None:
        baseline = TargetPolicyManifest(
            schema_version=1,
            allow_mutating_sessions=False,
            audit_capacity=512,
            max_snapshot_items=32,
            resources=[
                ResourcePolicyManifest(
                    id=0,
                    name="mmio0",
                    allow_unknown_reads=False,
                    allow_poll=False,
                    hard_denied=[[64, 128]],
                )
            ],
        )

        # 1. Enabling mutating sessions
        widened = TargetPolicyManifest(
            schema_version=1,
            allow_mutating_sessions=True,
            audit_capacity=512,
            max_snapshot_items=32,
            resources=baseline.resources,
        )
        with self.assertRaises(NarrowingError):
            baseline.narrow_with(widened)

        # 2. Increasing audit capacity
        widened = TargetPolicyManifest(
            schema_version=1,
            allow_mutating_sessions=False,
            audit_capacity=1024,
            max_snapshot_items=32,
            resources=baseline.resources,
        )
        with self.assertRaises(NarrowingError):
            baseline.narrow_with(widened)

        # 3. Increasing max snapshot items
        widened = TargetPolicyManifest(
            schema_version=1,
            allow_mutating_sessions=False,
            audit_capacity=512,
            max_snapshot_items=64,
            resources=baseline.resources,
        )
        with self.assertRaises(NarrowingError):
            baseline.narrow_with(widened)

        # 4. Enabling unknown reads
        widened = TargetPolicyManifest(
            schema_version=1,
            allow_mutating_sessions=False,
            audit_capacity=512,
            max_snapshot_items=32,
            resources=[
                ResourcePolicyManifest(
                    id=0,
                    name="mmio0",
                    allow_unknown_reads=True,
                    allow_poll=False,
                    hard_denied=[[64, 128]],
                )
            ],
        )
        with self.assertRaises(NarrowingError):
            baseline.narrow_with(widened)

        # 5. Enabling poll
        widened = TargetPolicyManifest(
            schema_version=1,
            allow_mutating_sessions=False,
            audit_capacity=512,
            max_snapshot_items=32,
            resources=[
                ResourcePolicyManifest(
                    id=0,
                    name="mmio0",
                    allow_unknown_reads=False,
                    allow_poll=True,
                    hard_denied=[[64, 128]],
                )
            ],
        )
        with self.assertRaises(NarrowingError):
            baseline.narrow_with(widened)

        # 6. Shrinking hard-denied range ([64, 100) does not cover [64, 128))
        widened = TargetPolicyManifest(
            schema_version=1,
            allow_mutating_sessions=False,
            audit_capacity=512,
            max_snapshot_items=32,
            resources=[
                ResourcePolicyManifest(
                    id=0,
                    name="mmio0",
                    allow_unknown_reads=False,
                    allow_poll=False,
                    hard_denied=[[64, 100]],
                )
            ],
        )
        with self.assertRaises(NarrowingError):
            baseline.narrow_with(widened)

    def test_writable_registers_narrowing_and_canonicalization(self) -> None:
        base_reg = WritableRegister(
            offset=0x10,
            allow_mask=0x0000FFFF,
            width=4,
            allow_rmw=False,
            require_precondition=True,
            precondition_mask=0x00000001,
            readback=True,
        )
        baseline = TargetPolicyManifest(
            schema_version=1,
            allow_mutating_sessions=True,
            audit_capacity=1024,
            max_snapshot_items=64,
            resources=[
                ResourcePolicyManifest(
                    id=0,
                    name="mmio0",
                    allow_unknown_reads=True,
                    allow_poll=False,
                    hard_denied=[],
                    writable_registers=[base_reg],
                )
            ],
        )

        # Runtime cannot add new register
        runtime = TargetPolicyManifest(
            schema_version=1,
            allow_mutating_sessions=True,
            audit_capacity=1024,
            max_snapshot_items=64,
            resources=[
                ResourcePolicyManifest(
                    id=0,
                    name="mmio0",
                    allow_unknown_reads=True,
                    allow_poll=False,
                    hard_denied=[],
                    writable_registers=[
                        base_reg,
                        WritableRegister(offset=0x20, allow_mask=0xFFFFFFFF),
                    ],
                )
            ],
        )
        with self.assertRaises(NarrowingError):
            baseline.narrow_with(runtime)

        # Runtime cannot widen allow_mask
        runtime = TargetPolicyManifest(
            schema_version=1,
            allow_mutating_sessions=True,
            audit_capacity=1024,
            max_snapshot_items=64,
            resources=[
                ResourcePolicyManifest(
                    id=0,
                    name="mmio0",
                    allow_unknown_reads=True,
                    allow_poll=False,
                    hard_denied=[],
                    writable_registers=[
                        WritableRegister(
                            offset=0x10,
                            allow_mask=0x0001FFFF,
                            width=4,
                            allow_rmw=False,
                            require_precondition=True,
                            precondition_mask=0x00000001,
                            readback=True,
                        )
                    ],
                )
            ],
        )
        with self.assertRaises(NarrowingError):
            baseline.narrow_with(runtime)

        # Runtime can narrow allow_mask
        runtime = TargetPolicyManifest(
            schema_version=1,
            allow_mutating_sessions=True,
            audit_capacity=1024,
            max_snapshot_items=64,
            resources=[
                ResourcePolicyManifest(
                    id=0,
                    name="mmio0",
                    allow_unknown_reads=True,
                    allow_poll=False,
                    hard_denied=[],
                    writable_registers=[
                        WritableRegister(
                            offset=0x10,
                            allow_mask=0x000000FF,
                            width=4,
                            allow_rmw=False,
                            require_precondition=True,
                            precondition_mask=0x00000001,
                            readback=True,
                        )
                    ],
                )
            ],
        )
        self.assertEqual(baseline.narrow_with(runtime), runtime)


if __name__ == "__main__":
    unittest.main()
