# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import tempfile
import unittest
from typing import Literal
from unittest import mock

import perf_publish.publish as perf_publish
from legacy_access_point.access_point import AccessPoint
from mobly import signals
from openwrt_access_point import OpenWrtAP
from openwrt_access_point.lib.access_point_config import Band, SecurityWpa2
from wlan_test_utils import throughput_utils


def _stat(val: float | None) -> throughput_utils.MetricStatsDict:
    return {"avg": val, "min": val, "max": val}


def _link_metrics(
    ap_tx_rate: float | None,
    ap_rx_rate: float | None,
) -> throughput_utils.LinkMetricsDict:
    return {
        "ap_rssi": _stat(-45.0),
        "ap_snr": _stat(40.0),
        "ap_tx_rate_mbps": _stat(ap_tx_rate),
        "ap_rx_rate_mbps": _stat(ap_rx_rate),
        "dut_rssi": _stat(-44.0),
        "dut_tx_rate_mbps": _stat(ap_rx_rate),
        "dut_rx_rate_mbps": _stat(None),
    }


def _make_measurements(
    *,
    phy_possible_mbps: float = 100.0,
    tcp_tx_phy: float | None = 90.0,
    tcp_rx_phy: float | None = 90.0,
    udp_tx_phy: float | None = 90.0,
    udp_rx_phy: float | None = 90.0,
    throughput_mbps: float = 65.0,
) -> throughput_utils.MeasurementsByDirection:
    raw_bps = int(throughput_mbps * 1_000_000)
    return {
        "tcp_tx": {
            "protocol": "tcp",
            "direction": "tx",
            "throughput_mbps": throughput_mbps,
            "phy_mode": "he",
            "nss": 2,
            "phy_possible_mbps": phy_possible_mbps,
            "link_metrics": _link_metrics(
                ap_tx_rate=90.0, ap_rx_rate=tcp_tx_phy
            ),
            "iperf_details": {
                "server_cpu_utilization_percent": 5.0,
                "raw_bps": raw_bps,
            },
        },
        "tcp_rx": {
            "protocol": "tcp",
            "direction": "rx",
            "throughput_mbps": throughput_mbps + 1.0,
            "phy_mode": "he",
            "nss": 2,
            "phy_possible_mbps": phy_possible_mbps,
            "link_metrics": _link_metrics(
                ap_tx_rate=tcp_rx_phy, ap_rx_rate=90.0
            ),
            "iperf_details": {
                "server_cpu_utilization_percent": 5.0,
                "raw_bps": raw_bps,
            },
        },
        "udp_tx": {
            "protocol": "udp",
            "direction": "tx",
            "throughput_mbps": throughput_mbps + 2.0,
            "phy_mode": "he",
            "nss": 2,
            "phy_possible_mbps": phy_possible_mbps,
            "link_metrics": _link_metrics(
                ap_tx_rate=90.0, ap_rx_rate=udp_tx_phy
            ),
            "iperf_details": {
                "server_cpu_utilization_percent": 5.0,
                "raw_bps": raw_bps,
                "corrected_bps": float(raw_bps),
                "loss_percent": 0.0,
            },
        },
        "udp_rx": {
            "protocol": "udp",
            "direction": "rx",
            "throughput_mbps": throughput_mbps + 3.0,
            "phy_mode": "he",
            "nss": 2,
            "phy_possible_mbps": phy_possible_mbps,
            "link_metrics": _link_metrics(
                ap_tx_rate=udp_rx_phy, ap_rx_rate=90.0
            ),
            "iperf_details": {
                "server_cpu_utilization_percent": 5.0,
                "raw_bps": raw_bps,
                "corrected_bps": float(raw_bps),
                "loss_percent": 0.0,
            },
        },
    }


def _test_params(
    band: Band,
    channel: int,
    bw: Literal[20, 40, 80, 160],
) -> throughput_utils.TestParams:
    return throughput_utils.TestParams(
        security_mode=SecurityWpa2(),
        band=band,
        channel=channel,
        channel_bandwidth=bw,
    )


class ThroughputUtilsTest(unittest.TestCase):
    def test_generate_test_name(self) -> None:
        params = throughput_utils.TestParams(
            security_mode=SecurityWpa2(),
            band=Band.BAND_2G,
            channel=1,
            channel_bandwidth=20,
        )
        self.assertEqual(
            throughput_utils.generate_test_name(params),
            "test_wpa2_channel_1_20mhz",
        )

    def test_build_default_test_params(self) -> None:
        params = throughput_utils.build_default_test_params()
        self.assertEqual(len(params), 3)
        channels = [p[0].channel for p in params]
        self.assertEqual(channels, [1, 165, 36])

    def test_resolve_ap_phy_caps(self) -> None:
        mock_openwrt = mock.MagicMock(spec=OpenWrtAP)
        mock_legacy = mock.MagicMock(spec=AccessPoint)

        caps_openwrt = throughput_utils.resolve_ap_phy_caps(mock_openwrt)
        self.assertEqual(caps_openwrt, throughput_utils.OPENWRT_ONE_CAPS)
        self.assertEqual(caps_openwrt.gen, throughput_utils.PhyGen.HE)

        caps_legacy = throughput_utils.resolve_ap_phy_caps(mock_legacy)
        self.assertEqual(caps_legacy, throughput_utils.WHIRLWIND_CAPS)
        self.assertEqual(caps_legacy.gen, throughput_utils.PhyGen.VHT)

    def test_build_test_case_record(self) -> None:
        measurements = _make_measurements()
        params = throughput_utils.TestParams(
            security_mode=SecurityWpa2(),
            band=Band.BAND_2G,
            channel=1,
            channel_bandwidth=20,
        )
        record_ap0 = throughput_utils.build_test_case_record(
            params, measurements, ap_name="ap0", is_primary_ap=True
        )
        self.assertEqual(record_ap0["ap"], "ap0")
        self.assertTrue(record_ap0["is_primary_ap"])
        self.assertEqual(record_ap0["channel"], 1)

        record_ap1 = throughput_utils.build_test_case_record(
            params, measurements, ap_name="ap1", is_primary_ap=False
        )
        self.assertEqual(record_ap1["ap"], "ap1")
        self.assertFalse(record_ap1["is_primary_ap"])
        self.assertEqual(record_ap1["channel"], 1)

    def test_throughput_reporter_single_ap(self) -> None:
        measurements = _make_measurements()
        params = throughput_utils.TestParams(
            security_mode=SecurityWpa2(),
            band=Band.BAND_2G,
            channel=1,
            channel_bandwidth=20,
        )

        with tempfile.TemporaryDirectory() as tmpdir:
            reporter = throughput_utils.ThroughputReporter(log_path=tmpdir)
            reporter.write_initial_json()
            reporter.record_test_case(params, measurements, ap_name="ap0")

            with mock.patch.object(perf_publish, "publish_fuchsiaperf"):
                reporter.finalize_reports()

            # Verify JSON report
            with open(reporter.json_file_path, "r", encoding="utf-8") as f:
                json_data = json.load(f)
            self.assertEqual(len(json_data["test_cases"]), 1)
            self.assertEqual(json_data["test_cases"][0]["ap"], "ap0")

            # Verify HTML report contains 'ap' column and throughput/phy columns
            with open(reporter.html_file_path, "r", encoding="utf-8") as f:
                html_content = f.read()
            self.assertIn("<th>ap</th>", html_content)
            self.assertIn("ap0", html_content)
            self.assertIn("<th>result_mbps</th>", html_content)
            self.assertIn("<th>phy_avg_mbps</th>", html_content)
            self.assertIn("<th>phy_avg_pct</th>", html_content)
            self.assertIn("<th>phy_possible_mbps</th>", html_content)

            # Verify fuchsiaperf report has un-prefixed labels
            with open(
                reporter.fuchsiaperf_file_path, "r", encoding="utf-8"
            ) as f:
                perf_data = json.load(f)

            labels = [entry["label"] for entry in perf_data]
            self.assertEqual(len(labels), 4)
            self.assertIn("ch1_20mhz/tcp_tx", labels)
            self.assertIn("ch1_20mhz/tcp_rx", labels)
            self.assertIn("ch1_20mhz/udp_tx", labels)
            self.assertIn("ch1_20mhz/udp_rx", labels)

    def test_throughput_reporter_multi_ap(self) -> None:
        measurements = _make_measurements()
        params = throughput_utils.TestParams(
            security_mode=SecurityWpa2(),
            band=Band.BAND_2G,
            channel=1,
            channel_bandwidth=20,
        )

        with tempfile.TemporaryDirectory() as tmpdir:
            reporter = throughput_utils.ThroughputReporter(log_path=tmpdir)
            reporter.write_initial_json()
            reporter.record_test_case(
                params, measurements, ap_name="ap0", is_primary_ap=True
            )
            reporter.record_test_case(
                params, measurements, ap_name="ap1", is_primary_ap=False
            )

            with mock.patch.object(perf_publish, "publish_fuchsiaperf"):
                reporter.finalize_reports()

            # Verify JSON report contains 'ap' field for both APs
            with open(reporter.json_file_path, "r", encoding="utf-8") as f:
                json_data = json.load(f)
            self.assertEqual(len(json_data["test_cases"]), 2)
            self.assertEqual(json_data["test_cases"][0]["ap"], "ap0")
            self.assertEqual(json_data["test_cases"][1]["ap"], "ap1")

            # Verify HTML report contains 'ap' column and rows for both APs
            with open(reporter.html_file_path, "r", encoding="utf-8") as f:
                html_content = f.read()
            self.assertIn("<th>ap</th>", html_content)
            self.assertIn("ap0", html_content)
            self.assertIn("ap1", html_content)

            # Verify fuchsiaperf report ONLY contains measurements for ap0!
            with open(
                reporter.fuchsiaperf_file_path, "r", encoding="utf-8"
            ) as f:
                perf_data = json.load(f)

            # Only ap0's 4 metrics should be published
            labels = [entry["label"] for entry in perf_data]
            self.assertEqual(len(labels), 4)
            self.assertIn("ch1_20mhz/tcp_tx", labels)
            self.assertIn("ch1_20mhz/tcp_rx", labels)
            self.assertIn("ch1_20mhz/udp_tx", labels)
            self.assertIn("ch1_20mhz/udp_rx", labels)

            # Ensure ap1 metrics are NOT published to fuchsiaperf
            self.assertFalse(any("ap1" in label for label in labels))

    def test_has_phy_rate_below_target_matches_assert_phy_rate_targets(
        self,
    ) -> None:
        passing = _make_measurements(
            phy_possible_mbps=100.0,
            tcp_tx_phy=80.0,
            tcp_rx_phy=85.0,
            udp_tx_phy=90.0,
            udp_rx_phy=80.0,
        )
        self.assertFalse(throughput_utils.has_phy_rate_below_target(passing))
        throughput_utils.assert_phy_rate_targets(passing, 100.0, "iris")

        failing_direction = _make_measurements(
            phy_possible_mbps=100.0,
            udp_rx_phy=79.9,
        )
        self.assertTrue(
            throughput_utils.has_phy_rate_below_target(failing_direction)
        )
        with self.assertRaises(signals.TestFailure):
            throughput_utils.assert_phy_rate_targets(
                failing_direction, 100.0, "iris"
            )

        missing_rate = _make_measurements(
            phy_possible_mbps=100.0,
            tcp_tx_phy=None,
        )
        self.assertTrue(
            throughput_utils.has_phy_rate_below_target(missing_rate)
        )
        with self.assertRaises(signals.TestFailure):
            throughput_utils.assert_phy_rate_targets(
                missing_rate, 100.0, "iris"
            )

    def test_finalize_reports_publishes_all_when_phy_rates_meet_target(
        self,
    ) -> None:
        with (
            tempfile.TemporaryDirectory() as tmpdir,
            mock.patch.object(
                perf_publish, "publish_fuchsiaperf"
            ) as mock_publish,
        ):
            reporter = throughput_utils.ThroughputReporter(
                tmpdir,
                fuchsiaperf_suite_name="fuchsia.wlan.throughput_test_adb",
            )
            reporter.write_initial_json()
            reporter.record_test_case(
                _test_params(Band.BAND_2G, 1, 20),
                _make_measurements(throughput_mbps=50.0),
                ap_name="ap0",
            )
            reporter.record_test_case(
                _test_params(Band.BAND_5G, 165, 20),
                _make_measurements(throughput_mbps=80.0),
                ap_name="ap0",
            )

            reporter.finalize_reports()

            mock_publish.assert_called_once_with(
                [reporter.fuchsiaperf_file_path],
                throughput_utils.EXPECTED_METRICS_FILENAME,
                runtime_deps_dir=tmpdir,
            )
            with open(reporter.fuchsiaperf_file_path, encoding="utf-8") as f:
                perf_entries = json.load(f)
            self.assertEqual(
                [entry["label"] for entry in perf_entries],
                [
                    "ch1_20mhz/tcp_tx",
                    "ch1_20mhz/tcp_rx",
                    "ch1_20mhz/udp_tx",
                    "ch1_20mhz/udp_rx",
                    "ch165_20mhz/tcp_tx",
                    "ch165_20mhz/tcp_rx",
                    "ch165_20mhz/udp_tx",
                    "ch165_20mhz/udp_rx",
                ],
            )
            self.assertTrue(
                all(
                    entry["test_suite"] == "fuchsia.wlan.throughput_test_adb"
                    for entry in perf_entries
                )
            )

    def test_finalize_reports_skips_fuchsiaperf_for_failed_phy_rate_cases(
        self,
    ) -> None:
        with (
            tempfile.TemporaryDirectory() as tmpdir,
            mock.patch.object(
                perf_publish, "publish_fuchsiaperf"
            ) as mock_publish,
        ):
            reporter = throughput_utils.ThroughputReporter(tmpdir)
            reporter.write_initial_json()
            # Ch 1 fails PHY target in a single direction; entire case is red.
            reporter.record_test_case(
                _test_params(Band.BAND_2G, 1, 20),
                _make_measurements(
                    phy_possible_mbps=100.0,
                    tcp_tx_phy=60.0,
                    throughput_mbps=30.0,
                ),
                ap_name="ap0",
            )
            # Ch 36 fails PHY target across directions.
            reporter.record_test_case(
                _test_params(Band.BAND_5G, 36, 80),
                _make_measurements(
                    phy_possible_mbps=1200.0,
                    tcp_tx_phy=500.0,
                    tcp_rx_phy=500.0,
                    udp_tx_phy=500.0,
                    udp_rx_phy=500.0,
                    throughput_mbps=200.0,
                ),
                ap_name="ap0",
            )
            # Ch 165 meets PHY targets in all directions.
            reporter.record_test_case(
                _test_params(Band.BAND_5G, 165, 20),
                _make_measurements(
                    phy_possible_mbps=100.0,
                    throughput_mbps=75.0,
                ),
                ap_name="ap0",
            )

            reporter.finalize_reports()

            mock_publish.assert_called_once_with(
                [reporter.fuchsiaperf_file_path],
                throughput_utils.EXPECTED_METRICS_FILENAME,
                runtime_deps_dir=tmpdir,
            )
            with open(reporter.fuchsiaperf_file_path, encoding="utf-8") as f:
                perf_entries = json.load(f)
            self.assertEqual(
                [entry["label"] for entry in perf_entries],
                [
                    "ch165_20mhz/tcp_tx",
                    "ch165_20mhz/tcp_rx",
                    "ch165_20mhz/udp_tx",
                    "ch165_20mhz/udp_rx",
                ],
            )

            expected_path = os.path.join(
                tmpdir, throughput_utils.EXPECTED_METRICS_FILENAME
            )
            with open(expected_path, encoding="utf-8") as f:
                expected_lines = f.read().splitlines()
            self.assertEqual(
                expected_lines,
                [
                    "[no-summarize-metrics]",
                    "fuchsia.wlan.throughput_test: ch165_20mhz/tcp_rx",
                    "fuchsia.wlan.throughput_test: ch165_20mhz/tcp_tx",
                    "fuchsia.wlan.throughput_test: ch165_20mhz/udp_rx",
                    "fuchsia.wlan.throughput_test: ch165_20mhz/udp_tx",
                ],
            )

            # JSON and HTML diagnostic reports still preserve all recorded cases.
            with open(reporter.json_file_path, encoding="utf-8") as f:
                json_report = json.load(f)
            self.assertEqual(
                [tc["channel"] for tc in json_report["test_cases"]],
                [1, 36, 165],
            )
            with open(reporter.html_file_path, encoding="utf-8") as f:
                html_report = f.read()
            self.assertIn('<tr class="fail">', html_report)

    def test_finalize_reports_skips_publish_when_all_cases_fail_phy_rate(
        self,
    ) -> None:
        with (
            tempfile.TemporaryDirectory() as tmpdir,
            mock.patch.object(
                perf_publish, "publish_fuchsiaperf"
            ) as mock_publish,
        ):
            reporter = throughput_utils.ThroughputReporter(tmpdir)
            reporter.write_initial_json()
            reporter.record_test_case(
                _test_params(Band.BAND_2G, 1, 20),
                _make_measurements(phy_possible_mbps=100.0, tcp_tx_phy=50.0),
                ap_name="ap0",
            )
            reporter.record_test_case(
                _test_params(Band.BAND_5G, 36, 80),
                _make_measurements(phy_possible_mbps=1200.0, udp_rx_phy=400.0),
                ap_name="ap0",
            )

            reporter.finalize_reports()

            mock_publish.assert_not_called()
            self.assertFalse(os.path.exists(reporter.fuchsiaperf_file_path))
            self.assertFalse(
                os.path.exists(
                    os.path.join(
                        tmpdir, throughput_utils.EXPECTED_METRICS_FILENAME
                    )
                )
            )
            self.assertTrue(os.path.exists(reporter.json_file_path))
            self.assertTrue(os.path.exists(reporter.html_file_path))

    def test_finalize_reports_from_json_fallback_filters_failed_cases(
        self,
    ) -> None:
        with (
            tempfile.TemporaryDirectory() as tmpdir,
            mock.patch.object(
                perf_publish, "publish_fuchsiaperf"
            ) as mock_publish,
        ):
            writer = throughput_utils.ThroughputReporter(tmpdir)
            writer.write_initial_json()
            writer.record_test_case(
                _test_params(Band.BAND_2G, 1, 20),
                _make_measurements(phy_possible_mbps=100.0, tcp_rx_phy=60.0),
                ap_name="ap0",
            )
            writer.record_test_case(
                _test_params(Band.BAND_5G, 165, 20),
                _make_measurements(phy_possible_mbps=100.0),
                ap_name="ap0",
            )

            reloaded = throughput_utils.ThroughputReporter(tmpdir)
            reloaded.finalize_reports()

            mock_publish.assert_called_once()
            with open(reloaded.fuchsiaperf_file_path, encoding="utf-8") as f:
                perf_entries = json.load(f)
            self.assertEqual(
                [entry["label"] for entry in perf_entries],
                [
                    "ch165_20mhz/tcp_tx",
                    "ch165_20mhz/tcp_rx",
                    "ch165_20mhz/udp_tx",
                    "ch165_20mhz/udp_rx",
                ],
            )

    def test_get_phy_avg_mbps(self) -> None:
        measurements = _make_measurements(
            tcp_tx_phy=85.0,
            tcp_rx_phy=95.0,
        )
        self.assertEqual(
            throughput_utils.get_phy_avg_mbps(measurements["tcp_tx"]), 85.0
        )
        self.assertEqual(
            throughput_utils.get_phy_avg_mbps(measurements["tcp_rx"]), 95.0
        )

        measurements["tcp_tx"]["link_metrics"]["ap_rx_rate_mbps"]["avg"] = None
        self.assertIsNone(
            throughput_utils.get_phy_avg_mbps(measurements["tcp_tx"])
        )

    def test_compute_phy_avg_pct(self) -> None:
        measurements = _make_measurements(
            phy_possible_mbps=100.0,
            tcp_tx_phy=80.0,
        )
        self.assertEqual(
            throughput_utils.compute_phy_avg_pct(measurements["tcp_tx"]), 80.0
        )

        measurements["tcp_tx"]["phy_possible_mbps"] = 0.0
        self.assertIsNone(
            throughput_utils.compute_phy_avg_pct(measurements["tcp_tx"])
        )

    def test_html_report_columns_and_order(self) -> None:
        measurements = _make_measurements(
            phy_possible_mbps=100.0,
            tcp_tx_phy=90.0,
            throughput_mbps=65.0,
        )
        params = _test_params(Band.BAND_2G, 1, 20)

        with tempfile.TemporaryDirectory() as tmpdir:
            reporter = throughput_utils.ThroughputReporter(log_path=tmpdir)
            reporter.write_initial_json()
            reporter.record_test_case(params, measurements, ap_name="ap0")
            with mock.patch.object(perf_publish, "publish_fuchsiaperf"):
                reporter.finalize_reports()

            with open(reporter.html_file_path, "r", encoding="utf-8") as f:
                html_content = f.read()

            expected_header_sequence = (
                "<th>result_mbps</th>\n"
                "        <th>phy_avg_mbps</th>\n"
                "        <th>phy_avg_pct</th>\n"
                "        <th>phy_possible_mbps</th>"
            )
            self.assertIn(expected_header_sequence, html_content)

            expected_row_sequence = (
                '<td class="num">65.0</td>\n'
                '        <td class="num">90.0</td>\n'
                '        <td class="num">90.0</td>\n'
                '        <td class="num">100.0</td>'
            )
            self.assertIn(expected_row_sequence, html_content)


if __name__ == "__main__":
    unittest.main()
