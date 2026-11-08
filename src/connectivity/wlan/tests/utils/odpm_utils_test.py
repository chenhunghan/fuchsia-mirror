# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import tempfile
import unittest
from unittest import mock

from honeydew.affordances.tracing import tracing as tracing_affordance
from honeydew.transports.ffx import types as ffx_types
from reporting import metrics
from trace_processing import trace_model, trace_time
from wlan_test_utils import odpm_utils


class OdpmUtilsTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self.mock_tracing = mock.create_autospec(tracing_affordance.Tracing)

    async def test_start_trace(self) -> None:
        self.mock_tracing.is_session_initialized.return_value = False
        self.mock_tracing.is_active.return_value = False
        await odpm_utils.start_trace(self.mock_tracing)
        self.mock_tracing.initialize.assert_called_once_with(
            categories=["kernel:meta", "power"],
        )
        self.mock_tracing.start.assert_awaited_once()

    async def test_start_trace_configures_and_logs_polling_once_per_tracing_instance(
        self,
    ) -> None:
        self.mock_tracing.is_session_initialized.return_value = False
        self.mock_tracing.is_active.return_value = False
        mock_ffx = mock.MagicMock()
        mock_ffx.run.side_effect = [
            f"bootstrap/archivist\n{odpm_utils.ODPM_MONIKER}\n",
            (
                "Successfully updated ODPM polling configuration.\n"
                "ODPM Polling Configuration:\n"
                "  Hardware poll interval:    1000.0 ms (1.000 s)\n"
                "  Polls per series entry:    1\n"
                "  Series recording interval: 1.00 s\n"
            ),
        ]
        self.mock_tracing._fc_transport = mock.MagicMock(
            _device_address_change=mock.MagicMock(ffx=mock_ffx)
        )

        with self.assertLogs(odpm_utils._LOGGER, level="DEBUG") as cm:
            await odpm_utils.start_trace(self.mock_tracing)
            await odpm_utils.start_trace(self.mock_tracing)

        self.assertEqual(
            mock_ffx.run.call_args_list,
            [
                mock.call(
                    ["component", "list"],
                    machine=ffx_types.MachineFormat.RAW,
                ),
                mock.call(
                    [
                        "component",
                        "explore",
                        odpm_utils.ODPM_MONIKER,
                        "--command",
                        "odpm_util config set --polls-per-series-entry 1 --poll-interval-ms 1000",
                    ],
                    machine=ffx_types.MachineFormat.RAW,
                ),
            ],
        )
        debug_msgs = [
            msg for msg in cm.output if "ODPM polling configuration:" in msg
        ]
        self.assertEqual(len(debug_msgs), 1)
        self.assertIn("Hardware poll interval:    1000.0 ms", debug_msgs[0])

    async def test_start_trace_raises_when_moniker_not_present(self) -> None:
        mock_ffx = mock.MagicMock()
        mock_ffx.run.return_value = "bootstrap/archivist\ncore/wlancfg\n"
        self.mock_tracing._fc_transport = mock.MagicMock(
            _device_address_change=mock.MagicMock(ffx=mock_ffx)
        )

        with self.assertRaisesRegex(RuntimeError, odpm_utils.ODPM_MONIKER):
            await odpm_utils.start_trace(self.mock_tracing)

        mock_ffx.run.assert_called_once_with(
            ["component", "list"],
            machine=ffx_types.MachineFormat.RAW,
        )
        self.mock_tracing.initialize.assert_not_called()
        self.mock_tracing.start.assert_not_awaited()

    async def test_record_odpm_metrics_skips_when_moniker_not_present(
        self,
    ) -> None:
        mock_ffx = mock.MagicMock()
        mock_ffx.run.return_value = "bootstrap/archivist\ncore/wlancfg\n"
        self.mock_tracing._fc_transport = mock.MagicMock(
            _device_address_change=mock.MagicMock(ffx=mock_ffx)
        )

        body_ran = False
        async with odpm_utils.record_odpm_metrics(
            self.mock_tracing, "/tmp/test_dir", "my_test"
        ):
            body_ran = True

        self.assertTrue(body_ran)
        mock_ffx.run.assert_called_once_with(
            ["component", "list"],
            machine=ffx_types.MachineFormat.RAW,
        )
        self.mock_tracing.initialize.assert_not_called()
        self.mock_tracing.start.assert_not_awaited()
        self.mock_tracing.stop.assert_not_awaited()
        self.mock_tracing.terminate_and_download.assert_not_awaited()

        # Subsequent calls on the same tracing instance should also skip without
        # calling ffx again.
        async with odpm_utils.record_odpm_metrics(
            self.mock_tracing, "/tmp/test_dir", "my_test_2"
        ):
            pass
        mock_ffx.run.assert_called_once()

    async def test_stop_trace(self) -> None:
        self.mock_tracing.is_active.return_value = True
        self.mock_tracing.terminate_and_download.return_value = (
            "/tmp/odpm_trace.fxt"
        )
        trace_path = await odpm_utils.stop_trace(self.mock_tracing, "/tmp")
        self.assertEqual(trace_path, "/tmp/odpm_trace.fxt")
        self.mock_tracing.stop.assert_awaited_once()
        self.mock_tracing.terminate_and_download.assert_awaited_once_with(
            directory="/tmp",
            trace_file="odpm_trace.fxt",
        )

    async def test_stop_trace_propagates_uninitialized_error(self) -> None:
        self.mock_tracing.is_active.return_value = False
        self.mock_tracing.terminate_and_download.side_effect = RuntimeError(
            "Trace session is not initialized"
        )
        with self.assertRaisesRegex(
            RuntimeError, "Trace session is not initialized"
        ):
            await odpm_utils.stop_trace(self.mock_tracing, "/tmp")

    @mock.patch("os.path.exists", return_value=True)
    @mock.patch(
        "wlan_test_utils.odpm_utils.trace_importing.create_model_from_trace_file_path"
    )
    @mock.patch(
        "wlan_test_utils.odpm_utils.odpm_power.OdpmPowerMetricsProcessor"
    )
    def test_get_odpm_measurements(
        self,
        mock_processor_class: mock.MagicMock,
        mock_create_model: mock.MagicMock,
        mock_os_exists: mock.MagicMock,
    ) -> None:
        mock_processor = mock_processor_class.return_value
        # Mock processor returning PowerAverage_wlan with duration-weighted mean 2.5W
        mock_processor.process_metrics.return_value = [
            metrics.TestCaseResult(
                label="PowerAverage_wlan",
                unit=metrics.Unit.watts,
                values=[2.5],
            )
        ]

        # Mock trace_model.CounterEvent for duration calculation (2 seconds)
        event1 = trace_model.CounterEvent(
            id=None,
            base=trace_model.Event(
                category="power",
                name="wlan_odpm_rail",
                start=trace_time.TimePoint(1000000000),
                pid=0,
                tid=0,
                args=None,
            ),
        )
        event2 = trace_model.CounterEvent(
            id=None,
            base=trace_model.Event(
                category="power",
                name="wlan_odpm_rail",
                start=trace_time.TimePoint(3000000000),
                pid=0,
                tid=0,
                args=None,
            ),
        )

        mock_model = mock.MagicMock()
        mock_model.all_events.return_value = [event1, event2]
        mock_create_model.return_value = mock_model

        results = odpm_utils.get_odpm_measurements("/tmp/odpm_trace.fxt")

        # We should have the Power metric (stripped of Average_wlan) and the calculated TotalEnergy
        self.assertEqual(len(results), 2)
        self.assertEqual(results[0].label, "Power")
        self.assertEqual(results[1].label, "TotalEnergy")
        self.assertEqual(results[1].unit, metrics.Unit.joules)

        # Duration is 2 seconds, mean power is 2.5W -> TotalEnergy is 5.0 Joules
        self.assertEqual(results[1].values, (5.0,))

    @mock.patch("os.path.exists", return_value=True)
    @mock.patch(
        "wlan_test_utils.odpm_utils.trace_importing.create_model_from_trace_file_path"
    )
    def test_get_odpm_measurements_real_processor(
        self,
        mock_create_model: mock.MagicMock,
        mock_os_exists: mock.MagicMock,
    ) -> None:
        """Verifies get_odpm_measurements using the real OdpmPowerMetricsProcessor (unmocked).

        The processor is initialized with sum_rails={"wlan": [rail]} (which generates
        "PowerAverage_wlan") and get_odpm_measurements normalizes the label to "Power".
        This unmocked test verifies that OdpmPowerMetricsProcessor and
        get_odpm_measurements produce "Power" and "TotalEnergy" as expected.
        """
        # Non-uniform intervals:
        #   t=1s: 1000mW (1.0W)
        #   t=2s (dt=1s): 2000mW (2.0W) -> 2.0 J
        #   t=5s (dt=3s): 4000mW (4.0W) -> 12.0 J
        # Duration-weighted mean power = (2.0 + 12.0) / 4.0 = 3.5W, TotalEnergy = 14.0 J
        event1 = trace_model.CounterEvent(
            id=None,
            base=trace_model.Event(
                category="power",
                name="wlan_bt_odpm_rail",
                start=trace_time.TimePoint(1000000000),
                pid=0,
                tid=0,
                args={"mW": 1000},
            ),
        )
        event2 = trace_model.CounterEvent(
            id=None,
            base=trace_model.Event(
                category="power",
                name="wlan_bt_odpm_rail",
                start=trace_time.TimePoint(2000000000),
                pid=0,
                tid=0,
                args={"mW": 2000},
            ),
        )
        event3 = trace_model.CounterEvent(
            id=None,
            base=trace_model.Event(
                category="power",
                name="wlan_bt_odpm_rail",
                start=trace_time.TimePoint(5000000000),
                pid=0,
                tid=0,
                args={"mW": 4000},
            ),
        )

        model = trace_model.Model(
            processes=[
                trace_model.Process(
                    pid=0,
                    threads=[
                        trace_model.Thread(
                            tid=0, events=[event1, event2, event3]
                        )
                    ],
                )
            ],
            scheduling_records={},
        )
        mock_create_model.return_value = model

        results = odpm_utils.get_odpm_measurements("/tmp/odpm_trace.fxt")

        # Verify real OdpmPowerMetricsProcessor + get_odpm_measurements produced Power and TotalEnergy
        self.assertEqual(len(results), 2)
        self.assertEqual(results[0].label, "Power")
        self.assertEqual(results[0].values, (3.5,))
        self.assertEqual(results[1].label, "TotalEnergy")
        self.assertEqual(results[1].unit, metrics.Unit.joules)
        self.assertEqual(results[1].values, (14.0,))

    @mock.patch("os.path.exists", return_value=False)
    def test_get_odpm_measurements_raises_when_file_missing(
        self,
        mock_os_exists: mock.MagicMock,
    ) -> None:
        with self.assertRaises(FileNotFoundError):
            odpm_utils.get_odpm_measurements("/tmp/missing.fxt")

    @mock.patch("os.path.exists", return_value=True)
    @mock.patch(
        "wlan_test_utils.odpm_utils.trace_importing.create_model_from_trace_file_path"
    )
    def test_get_odpm_measurements_raises_when_no_measurements(
        self,
        mock_create_model: mock.MagicMock,
        mock_os_exists: mock.MagicMock,
    ) -> None:
        mock_create_model.return_value = trace_model.Model(
            processes=[],
            scheduling_records={},
        )
        with self.assertRaisesRegex(ValueError, "Missing ODPM power samples"):
            odpm_utils.get_odpm_measurements("/tmp/empty_odpm.fxt")

    @mock.patch("os.path.exists", return_value=True)
    @mock.patch("os.remove")
    @mock.patch("wlan_test_utils.odpm_utils.publish.publish_fuchsiaperf")
    @mock.patch("reporting.metrics.TestCaseResult.write_fuchsiaperf_json")
    @mock.patch("wlan_test_utils.odpm_utils.get_odpm_measurements")
    @mock.patch("wlan_test_utils.odpm_utils.stop_trace")
    async def test_stop_trace_and_save_metrics(
        self,
        mock_stop_trace: mock.AsyncMock,
        mock_get_measurements: mock.MagicMock,
        mock_write_json: mock.MagicMock,
        mock_publish_fuchsiaperf: mock.MagicMock,
        mock_os_remove: mock.MagicMock,
        mock_os_exists: mock.MagicMock,
    ) -> None:
        mock_stop_trace.return_value = "/tmp/test_dir/odpm_trace.fxt"
        expected_metrics = [
            metrics.TestCaseResult(
                label="Power",
                unit=metrics.Unit.watts,
                values=[2.5],
            )
        ]
        mock_get_measurements.return_value = expected_metrics

        res = await odpm_utils.stop_trace_and_save_metrics(
            self.mock_tracing, "/tmp/test_dir", "my_test", save_raw_trace=False
        )
        self.assertEqual(res, expected_metrics)
        mock_stop_trace.assert_awaited_once_with(
            self.mock_tracing, trace_output_dir="/tmp/test_dir"
        )
        mock_get_measurements.assert_called_once_with(
            "/tmp/test_dir/odpm_trace.fxt",
            rail=odpm_utils.ODPM_WLAN_RAIL,
        )
        mock_write_json.assert_called_once()
        mock_publish_fuchsiaperf.assert_called_once()
        mock_os_remove.assert_called_once_with("/tmp/test_dir/odpm_trace.fxt")

    @mock.patch("os.path.exists", return_value=True)
    @mock.patch("os.remove")
    @mock.patch("wlan_test_utils.odpm_utils.publish.publish_fuchsiaperf")
    @mock.patch(
        "wlan_test_utils.odpm_utils.get_odpm_measurements",
        side_effect=ValueError("Parse error"),
    )
    @mock.patch("wlan_test_utils.odpm_utils.stop_trace")
    async def test_stop_trace_and_save_metrics_propagates_parse_exception(
        self,
        mock_stop_trace: mock.AsyncMock,
        mock_get_measurements: mock.MagicMock,
        mock_publish_fuchsiaperf: mock.MagicMock,
        mock_os_remove: mock.MagicMock,
        mock_os_exists: mock.MagicMock,
    ) -> None:
        mock_stop_trace.return_value = "/tmp/test_dir/odpm_trace.fxt"
        with self.assertRaises(ValueError):
            await odpm_utils.stop_trace_and_save_metrics(
                self.mock_tracing,
                "/tmp/test_dir",
                "my_test",
                save_raw_trace=False,
            )
        mock_publish_fuchsiaperf.assert_not_called()
        mock_os_remove.assert_called_once_with("/tmp/test_dir/odpm_trace.fxt")

    @mock.patch("os.path.exists", return_value=True)
    @mock.patch("os.remove")
    @mock.patch("wlan_test_utils.odpm_utils.publish.publish_fuchsiaperf")
    @mock.patch(
        "reporting.metrics.TestCaseResult.write_fuchsiaperf_json",
        side_effect=OSError("Disk full"),
    )
    @mock.patch("wlan_test_utils.odpm_utils.get_odpm_measurements")
    @mock.patch("wlan_test_utils.odpm_utils.stop_trace")
    async def test_stop_trace_and_save_metrics_propagates_write_exception(
        self,
        mock_stop_trace: mock.AsyncMock,
        mock_get_measurements: mock.MagicMock,
        mock_write_json: mock.MagicMock,
        mock_publish_fuchsiaperf: mock.MagicMock,
        mock_os_remove: mock.MagicMock,
        mock_os_exists: mock.MagicMock,
    ) -> None:
        mock_stop_trace.return_value = "/tmp/test_dir/odpm_trace.fxt"
        expected_metrics = [
            metrics.TestCaseResult(
                label="Power",
                unit=metrics.Unit.watts,
                values=[2.5],
            )
        ]
        mock_get_measurements.return_value = expected_metrics

        with self.assertRaises(OSError):
            await odpm_utils.stop_trace_and_save_metrics(
                self.mock_tracing,
                "/tmp/test_dir",
                "my_test",
                save_raw_trace=False,
            )
        mock_publish_fuchsiaperf.assert_not_called()
        mock_os_remove.assert_called_once_with("/tmp/test_dir/odpm_trace.fxt")

    @mock.patch("wlan_test_utils.odpm_utils.stop_trace_and_save_metrics")
    @mock.patch("wlan_test_utils.odpm_utils.start_trace")
    async def test_record_odpm_metrics(
        self,
        mock_start_trace: mock.AsyncMock,
        mock_stop_and_save: mock.AsyncMock,
    ) -> None:
        async with odpm_utils.record_odpm_metrics(
            self.mock_tracing, "/tmp/test_dir", "my_test"
        ):
            mock_start_trace.assert_awaited_once_with(self.mock_tracing)
            mock_stop_and_save.assert_not_awaited()

        mock_stop_and_save.assert_awaited_once_with(
            self.mock_tracing,
            test_case_path="/tmp/test_dir",
            test_name="my_test",
            rail=odpm_utils.ODPM_WLAN_RAIL,
            save_raw_trace=False,
        )

    @mock.patch("wlan_test_utils.odpm_utils.stop_trace")
    @mock.patch("wlan_test_utils.odpm_utils.stop_trace_and_save_metrics")
    @mock.patch("wlan_test_utils.odpm_utils.start_trace")
    async def test_record_odpm_metrics_terminates_without_publishing_on_exception(
        self,
        mock_start_trace: mock.AsyncMock,
        mock_stop_and_save: mock.AsyncMock,
        mock_stop_trace: mock.AsyncMock,
    ) -> None:
        with self.assertRaises(RuntimeError):
            async with odpm_utils.record_odpm_metrics(
                self.mock_tracing, "/tmp/test_dir", "my_test"
            ):
                raise RuntimeError("Test failure")

        mock_start_trace.assert_awaited_once_with(self.mock_tracing)
        mock_stop_and_save.assert_not_awaited()
        mock_stop_trace.assert_not_awaited()
        self.mock_tracing.terminate.assert_awaited_once_with()

    @mock.patch("wlan_test_utils.odpm_utils.stop_trace_and_save_metrics")
    @mock.patch("wlan_test_utils.odpm_utils.start_trace")
    async def test_record_odpm_metrics_preserves_original_exception_when_cleanup_fails(
        self,
        mock_start_trace: mock.AsyncMock,
        mock_stop_and_save: mock.AsyncMock,
    ) -> None:
        self.mock_tracing.terminate.side_effect = ConnectionError(
            "Device offline"
        )
        with self.assertLogs(odpm_utils._LOGGER, level="WARNING") as cm:
            with self.assertRaisesRegex(RuntimeError, "Test failure"):
                async with odpm_utils.record_odpm_metrics(
                    self.mock_tracing, "/tmp/test_dir", "my_test"
                ):
                    raise RuntimeError("Test failure")

        mock_start_trace.assert_awaited_once_with(self.mock_tracing)
        mock_stop_and_save.assert_not_awaited()
        self.mock_tracing.terminate.assert_awaited_once_with()
        self.assertTrue(
            any("Device offline" in msg for msg in cm.output),
            f"Expected warning containing 'Device offline', got {cm.output}",
        )

    @mock.patch("wlan_test_utils.odpm_utils.get_odpm_measurements")
    @mock.patch("wlan_test_utils.odpm_utils.stop_trace")
    async def test_stop_trace_and_save_metrics_real_publish(
        self,
        mock_stop_trace: mock.AsyncMock,
        mock_get_measurements: mock.MagicMock,
    ) -> None:
        """Verifies real publish_fuchsiaperf integration against fuchsia.wlan.test_scan_while_connected.txt."""
        with tempfile.TemporaryDirectory() as temp_dir:
            trace_file = os.path.join(temp_dir, "odpm_trace.fxt")
            with open(trace_file, "w") as f:
                f.write("")
            mock_stop_trace.return_value = trace_file
            expected_metrics = [
                metrics.TestCaseResult(
                    label="Power",
                    unit=metrics.Unit.watts,
                    values=[2.0, 3.0],
                ),
                metrics.TestCaseResult(
                    label="TotalEnergy",
                    unit=metrics.Unit.joules,
                    values=[5.0],
                ),
            ]
            mock_get_measurements.return_value = expected_metrics

            res = await odpm_utils.stop_trace_and_save_metrics(
                self.mock_tracing,
                temp_dir,
                "test_scan_while_connected",
                save_raw_trace=False,
            )
            self.assertEqual(res, expected_metrics)
            results_path = os.path.join(temp_dir, "results.fuchsiaperf.json")
            self.assertTrue(os.path.exists(results_path))
            self.assertTrue(
                os.path.exists(
                    os.path.join(temp_dir, "results.catapult_json_disabled")
                )
            )
            with open(results_path, "r") as f:
                data = json.load(f)
            self.assertEqual(len(data), 2)
            self.assertEqual(
                {entry["label"] for entry in data},
                {"Power", "TotalEnergy"},
            )
            self.assertEqual(
                {entry["test_suite"] for entry in data},
                {"fuchsia.wlan.test_scan_while_connected"},
            )

    @mock.patch("wlan_test_utils.odpm_utils.get_odpm_measurements")
    @mock.patch("wlan_test_utils.odpm_utils.stop_trace")
    async def test_stop_trace_and_save_metrics_fails_when_expected_metric_missing(
        self,
        mock_stop_trace: mock.AsyncMock,
        mock_get_measurements: mock.MagicMock,
    ) -> None:
        """Verifies missing expected metrics fail allowlist check since there are no [optional] tags."""
        with tempfile.TemporaryDirectory() as temp_dir:
            trace_file = os.path.join(temp_dir, "odpm_trace.fxt")
            with open(trace_file, "w") as f:
                f.write("")
            mock_stop_trace.return_value = trace_file
            mock_get_measurements.return_value = [
                metrics.TestCaseResult(
                    label="Power",
                    unit=metrics.Unit.watts,
                    values=[2.0],
                ),
            ]

            with self.assertRaises(ValueError):
                await odpm_utils.stop_trace_and_save_metrics(
                    self.mock_tracing,
                    temp_dir,
                    "test_scan_while_connected",
                    save_raw_trace=False,
                )


if __name__ == "__main__":
    unittest.main()
