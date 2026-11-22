# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Test for profile.py"""

import unittest
from unittest import mock

from memory import profile
from reporting import metrics

MM2_OUTPUT = """
    {
        "ComponentDigest": {
            "kernel": {
                "memory_statistics": {
                    "total_bytes": 8588746752,
                    "free_bytes": 5056327680
                },
                "compression_statistics": {
                    "uncompressed_storage_bytes": 0
                }
            },
            "principals": [
                {
                    "id": 5,
                    "name": "bootstrap/fshost/fxfs",
                    "principal_type": "R",
                    "committed_private": 32927744,
                    "committed_scaled": 450043948.85779566,
                    "committed_total": 1436753920,
                    "populated_private": 32927744,
                    "populated_scaled": 450043948.85779566,
                    "populated_total": 1436753920,
                    "attributor": "root",
                    "processes": [
                        "fxfs.cm (13934)"
                    ],
                    "vmos": {
                        "[blobs]": {
                            "count": 1827,
                            "committed_private": 0,
                            "committed_scaled": 411303842.98151606,
                            "committed_total": 1385910272,
                            "populated_private": 0,
                            "populated_scaled": 411303842.98151606,
                            "populated_total": 1385910272
                        }
                    }
                }
            ],
            "undigested": 0,
            "digest": {
                "buckets": [
                    {
                        "name": "ZBI Buffer",
                        "committed_size": 0,
                        "populated_size": 0
                    },
                    {
                        "name": "[Addl]Graphics",
                        "populated_size": 65432,
                        "committed_size": 49152
                    }
                ]
            }
        }
    }
"""


class ProfileTest(unittest.TestCase):
    def test_capture_and_export_digest(self) -> None:
        def ffx_run_fake_implementation(args: list[str]) -> str:
            return MM2_OUTPUT

        dut = mock.Mock()
        dut.ffx.run.side_effect = ffx_run_fake_implementation
        report = profile.capture(
            dut, buckets_metrics=".*Graphics", board_memory=8 * 1024**3
        )

        self.assertEqual(
            report.structured,
            [
                metrics.TestCaseResult(
                    label="Memory/Bucket/AddlGraphics/CommittedBytes",
                    unit=metrics.Unit.bytes,
                    values=(49152,),
                    doc="Total committed bytes in the bucket: [Addl]Graphics",
                ),
                metrics.TestCaseResult(
                    label="Memory/Bucket/AddlGraphics/PopulatedBytes",
                    unit=metrics.Unit.bytes,
                    values=(65432,),
                    doc="Total populated bytes in the bucket: [Addl]Graphics",
                ),
                metrics.TestCaseResult(
                    label="Memory/System/FuchsiaOSPopulatedBytes",
                    unit=metrics.Unit.bytes,
                    values=(1187840,),
                    doc="Fuchsia OS Populated Memory",
                ),
            ],
        )

    @mock.patch.object(profile, "time", autospec=True, spec_set=True)
    def test_elapsed_time(self, mock_time: mock.MagicMock) -> None:
        mock_time.perf_counter.side_effect = (1.0, 1.5)
        dut = mock.Mock()
        dut.ffx.run.return_value = MM2_OUTPUT
        report = profile.capture(dut, measure_elapsed_time=True)

        elapsed_metrics = tuple(
            m
            for m in report.structured
            if m.label == "Memory/Tool/MemoryProfileElapsedMs"
        )
        self.assertSequenceEqual(
            elapsed_metrics,
            (
                metrics.TestCaseResult(
                    label="Memory/Tool/MemoryProfileElapsedMs",
                    unit=metrics.Unit.milliseconds,
                    values=(500.0,),
                    doc="Wall-clock time taken by 'ffx profile memory' to complete",
                ),
            ),
        )

    def test_capture_abridged(self) -> None:
        abridged_output = MM2_OUTPUT.replace(
            '"ComponentDigest":', '"Summary":', 1
        )

        def ffx_run_fake_implementation(args: list[str]) -> str:
            self.assertEqual(
                args,
                [
                    "--machine",
                    "json",
                    "-c",
                    "ffx_profile_memory_components=true",
                    "profile",
                    "memory",
                    "components",
                    "--buckets",
                    "--abridged",
                ],
            )
            return abridged_output

        dut = mock.Mock()
        dut.ffx.run.side_effect = ffx_run_fake_implementation
        report = profile.capture(
            dut,
            principal_groups={"fxfs": "*/fxfs"},
            board_memory=8 * 1024**3,
            abridged=True,
        )

        self.assertEqual(
            report.structured,
            [
                metrics.TestCaseResult(
                    label="Memory/Principal/fxfs/PrivatePopulated",
                    unit=metrics.Unit.bytes,
                    values=(32927744,),
                    doc="Total populated bytes for private uncompressed memory "
                    "VMOs: fxfs",
                ),
                metrics.TestCaseResult(
                    label="Memory/System/FuchsiaOSPopulatedBytes",
                    unit=metrics.Unit.bytes,
                    values=(1187840,),
                    doc="Fuchsia OS Populated Memory",
                ),
            ],
        )

    def test_capture_and_compute_metrics(self) -> None:
        def ffx_run_fake_implementation(args: list[str]) -> str:
            backend = args[args.index("--backend") + 1]
            if backend == "memory_monitor_1":
                raise RuntimeError("Boom")
            return MM2_OUTPUT

        dut = mock.Mock()
        dut.ffx.run.side_effect = ffx_run_fake_implementation
        report = profile.capture(
            dut, principal_groups={"fxfs": "*/fxfs"}, board_memory=8 * 1024**3
        )

        self.assertEqual(
            report.structured,
            [
                metrics.TestCaseResult(
                    label="Memory/Principal/fxfs/PrivatePopulated",
                    unit=metrics.Unit.bytes,
                    values=(32927744,),
                    doc="Total populated bytes for private uncompressed memory "
                    "VMOs: fxfs",
                ),
                metrics.TestCaseResult(
                    label="Memory/System/FuchsiaOSPopulatedBytes",
                    unit=metrics.Unit.bytes,
                    values=(1187840,),
                    doc="Fuchsia OS Populated Memory",
                ),
            ],
        )
        self.maxDiff = None
        self.assertEqual(
            report.freeform,
            {
                "memory_profile": {
                    "kernel": {
                        "memory_statistics": {
                            "total_bytes": 8588746752,
                            "free_bytes": 5056327680,
                        },
                        "compression_statistics": {
                            "uncompressed_storage_bytes": 0,
                        },
                    },
                    "principals": [
                        {
                            "id": 5,
                            "name": "bootstrap/fshost/fxfs",
                            "principal_type": "R",
                            "committed_private": 32927744,
                            "committed_scaled": 450043948.85779566,
                            "committed_total": 1436753920,
                            "populated_private": 32927744,
                            "populated_scaled": 450043948.85779566,
                            "populated_total": 1436753920,
                            "attributor": "root",
                            "processes": ["fxfs.cm (13934)"],
                            "vmos": [
                                {
                                    "name": "[blobs]",
                                    "count": 1827,
                                    "committed_private": 0,
                                    "committed_scaled": 411303842.98151606,
                                    "committed_total": 1385910272,
                                    "populated_private": 0,
                                    "populated_scaled": 411303842.98151606,
                                    "populated_total": 1385910272,
                                }
                            ],
                        }
                    ],
                    "undigested": 0,
                    "digest": {
                        "buckets": [
                            {
                                "name": "ZBI Buffer",
                                "populated_size": 0,
                                "committed_size": 0,
                            },
                            {
                                "name": "[Addl]Graphics",
                                "committed_size": 49152,
                                "populated_size": 65432,
                            },
                        ]
                    },
                }
            },
        )

    def test_fuchsia_os_populated_bytes_independent_clamping(self) -> None:
        anon_bucket = {
            "name": "[Addl]PopulatedAnonymousBytes",
            "committed_size": 500_000_000,
            "populated_size": 500_000_000,
        }
        component_profile = {
            "ComponentDigest": {
                "kernel": {
                    "memory_statistics": {
                        "total_bytes": 8588746752,
                    },
                },
                "principals": [],
                "digest": {
                    "buckets": [
                        anon_bucket,
                        {
                            "name": "StarnixContainer",
                            "committed_size": 200_000_000,
                            "populated_size": 200_000_000,
                        },
                        {
                            "name": "EngTools",
                            "committed_size": 50_000_000,
                            "populated_size": 50_000_000,
                        },
                    ]
                },
            }
        }

        # When board_memory < total_bytes, reserved_memory_bytes clamps to 0
        # without reducing fuchsia_anon_memory_bytes (500M - 200M - 50M = 250M).
        results_underestimated_board = profile.process_component_profile(
            principal_groups={},
            buckets_metrics=None,
            component_profile=component_profile,
            board_memory=2 * 1024**3,
        )
        self.assertEqual(
            results_underestimated_board,
            [
                metrics.TestCaseResult(
                    label="Memory/System/FuchsiaOSPopulatedBytes",
                    unit=metrics.Unit.bytes,
                    values=[250_000_000],
                    doc="Fuchsia OS Populated Memory",
                ),
            ],
        )

        # When starnix_bytes + eng_tools_bytes > addl_anon_bytes,
        # fuchsia_anon_memory_bytes clamps to 0 without reducing
        # reserved_memory_bytes (8 GiB - 8588746752 = 1187840).
        anon_bucket["populated_size"] = 100_000_000
        results_negative_anon = profile.process_component_profile(
            principal_groups={},
            buckets_metrics=None,
            component_profile=component_profile,
            board_memory=8 * 1024**3,
        )
        self.assertEqual(
            results_negative_anon,
            [
                metrics.TestCaseResult(
                    label="Memory/System/FuchsiaOSPopulatedBytes",
                    unit=metrics.Unit.bytes,
                    values=[1187840],
                    doc="Fuchsia OS Populated Memory",
                ),
            ],
        )

    def test_fuchsia_os_populated_bytes_excludes_erofs_and_ext4(self) -> None:
        component_profile = {
            "ComponentDigest": {
                "kernel": {
                    "memory_statistics": {
                        "total_bytes": 8588746752,
                    },
                },
                "principals": [],
                "digest": {
                    "buckets": [
                        {
                            "name": "[Addl]PopulatedAnonymousBytes",
                            "committed_size": 500_000_000,
                            "populated_size": 500_000_000,
                        },
                        {
                            "name": "StarnixContainer",
                            "committed_size": 200_000_000,
                            "populated_size": 200_000_000,
                        },
                        {
                            "name": "Erofs",
                            "committed_size": 120_000_000,
                            "populated_size": 120_000_000,
                        },
                        {
                            "name": "Ext4",
                            "committed_size": 30_000_000,
                            "populated_size": 30_000_000,
                        },
                        {
                            "name": "EngTools",
                            "committed_size": 50_000_000,
                            "populated_size": 50_000_000,
                        },
                    ]
                },
            }
        }

        # reserved (8 GiB - 8588746752 = 1187840) +
        # anon (500M - 200M - 120M - 30M - 50M = 100M) = 101_187_840.
        results = profile.process_component_profile(
            principal_groups={},
            buckets_metrics=None,
            component_profile=component_profile,
            board_memory=8 * 1024**3,
        )
        self.assertEqual(
            results,
            [
                metrics.TestCaseResult(
                    label="Memory/System/FuchsiaOSPopulatedBytes",
                    unit=metrics.Unit.bytes,
                    values=[101_187_840],
                    doc="Fuchsia OS Populated Memory",
                ),
            ],
        )
