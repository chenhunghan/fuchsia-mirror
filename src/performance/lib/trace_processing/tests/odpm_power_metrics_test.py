#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for ../metrics/odpm_power.py."""

import unittest

from reporting import metrics
from trace_processing import trace_model
from trace_processing.metrics import odpm_power

U = metrics.Unit
TestCaseResult = metrics.TestCaseResult


class OdpmPowerMetricsTest(unittest.TestCase):
    """ODPM power metrics tests."""

    def construct_trace_model(self) -> trace_model.Model:
        """Builds a fake trace model with ODPM power CounterEvents."""
        events: list[trace_model.Event] = [
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "other_category",
                    "name": "cpu_big_odpm_rail",
                    "ts": 1000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 1000.0},  # 1.0 W
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "other_category",
                    "name": "cpu_big_odpm_rail",
                    "ts": 2000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 3000.0},  # 3.0 W
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "gpu_odpm_rail",
                    "ts": 1000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 500.0},  # 0.5 W
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "gpu_odpm_rail",
                    "ts": 2000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 1500.0},  # 1.5 W
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "wlan_bt_odpm_rail",
                    "ts": 1000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 250.0},  # 0.25 W
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "wlan_bt_odpm_rail",
                    "ts": 2000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 750.0},  # 0.75 W
                }
            ),
            # Counter event without _odpm_rail suffix should be ignored
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "non_odpm_counter",
                    "ts": 1000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 500.0},
                }
            ),
            # Non-ODPM power event (missing mW arg) should be ignored
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "other_counter_odpm_rail",
                    "ts": 1000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"Voltage": 12.0},
                }
            ),
        ]

        odpm_process = trace_model.Process(
            100,
            "odpm.cm",
            [trace_model.Thread(101, "fdf-dispatcher-thread-0", events)],
        )
        return trace_model.Model([odpm_process], {})

    def test_empty_rails_raises_value_error(self) -> None:
        """Verifies that invalid rail / all_rails / sum_rails combinations raise ValueError."""
        with self.assertRaises(ValueError):
            odpm_power.OdpmPowerMetricsProcessor(rails=[])
        with self.assertRaises(ValueError):
            odpm_power.OdpmPowerMetricsProcessor()
        with self.assertRaises(ValueError):
            odpm_power.OdpmPowerMetricsProcessor(
                rails=["cpu_big"], all_rails=True
            )
        with self.assertRaises(ValueError):
            odpm_power.OdpmPowerMetricsProcessor(sum_rails={"empty_group": []})

    def test_trace_import_filter_modes(self) -> None:
        """Verifies trace import filtering uses _odpm_rail event patterns and no category filter."""
        processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=["gpu", "cpu_big"]
        )
        self.assertEqual(
            processor.event_patterns, {"gpu_odpm_rail", "cpu_big_odpm_rail"}
        )
        self.assertEqual(processor.category_names, set())

        sum_rails_processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=["wlan_bt"],
            sum_rails={"compute": ["cpu_big", "gpu"]},
        )
        self.assertEqual(
            sum_rails_processor.event_patterns,
            {"wlan_bt_odpm_rail", "cpu_big_odpm_rail", "gpu_odpm_rail"},
        )
        self.assertEqual(sum_rails_processor.category_names, set())

        all_rails_processor = odpm_power.OdpmPowerMetricsProcessor(
            all_rails=True
        )
        self.assertEqual(all_rails_processor.event_patterns, {r".*_odpm_rail"})
        self.assertEqual(all_rails_processor.category_names, set())

        predicate_sum_processor = odpm_power.OdpmPowerMetricsProcessor(
            sum_rails={"all_rails_except_battery": lambda r: r != "battery"},
        )
        self.assertEqual(
            predicate_sum_processor.event_patterns, {r".*_odpm_rail"}
        )
        self.assertEqual(predicate_sum_processor.category_names, set())

    def test_list_rails(self) -> None:
        """Verifies OdpmPowerMetricsProcessor.list_rails discovers available ODPM rails in a model."""
        model = self.construct_trace_model()
        self.assertEqual(
            odpm_power.OdpmPowerMetricsProcessor.list_rails(model),
            ["cpu_big", "gpu", "wlan_bt"],
        )

    def test_process_metrics_selected_rails(self) -> None:
        """Verifies metrics calculation only for selected rails in order."""
        model = self.construct_trace_model()
        processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=["gpu", "cpu_big"]
        )
        results = processor.process_metrics(model)

        expected = [
            TestCaseResult(
                label="Power_rail_gpu",
                unit=U.watts,
                values=[0.5, 1.5],
                doc="ODPM power usage samples for rail gpu",
            ),
            TestCaseResult(
                label="Power_rail_cpu_big",
                unit=U.watts,
                values=[1.0, 3.0],
                doc="ODPM power usage samples for rail cpu_big",
            ),
        ]
        self.assertEqual(results, expected)

    def test_process_metrics_all_rails(self) -> None:
        """Verifies all_rails=True reports metrics for all available rails in sorted order."""
        model = self.construct_trace_model()
        processor = odpm_power.OdpmPowerMetricsProcessor(all_rails=True)
        results = processor.process_metrics(model)

        expected = [
            TestCaseResult(
                label="Power_rail_cpu_big",
                unit=U.watts,
                values=[1.0, 3.0],
                doc="ODPM power usage samples for rail cpu_big",
            ),
            TestCaseResult(
                label="Power_rail_gpu",
                unit=U.watts,
                values=[0.5, 1.5],
                doc="ODPM power usage samples for rail gpu",
            ),
            TestCaseResult(
                label="Power_rail_wlan_bt",
                unit=U.watts,
                values=[0.25, 0.75],
                doc="ODPM power usage samples for rail wlan_bt",
            ),
        ]
        self.assertEqual(results, expected)

    def test_missing_rail_skipped(self) -> None:
        """Verifies missing rails are skipped and empty result returned if none match."""
        model = self.construct_trace_model()
        processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=["nonexistent_rail"]
        )
        self.assertEqual(processor.process_metrics(model), [])

    def test_sum_rails_sample_count_discrepancy(self) -> None:
        """Verifies discrepancy <= 2 truncates with a warning while > 2 raises ValueError."""
        model = self.construct_trace_model()
        thread = model.processes[0].threads[0]
        # Add 2 extra samples to cpu_big (total 4 vs gpu's 2 -> discrepancy of 2, allowed)
        for ts in (3000, 4000):
            thread.events.append(
                trace_model.CounterEvent.consume_dict(
                    {
                        "cat": "power",
                        "name": "cpu_big_odpm_rail",
                        "ts": ts,
                        "pid": 100,
                        "tid": 101,
                        "args": {"mW": 5000.0},
                    }
                )
            )

        processor = odpm_power.OdpmPowerMetricsProcessor(
            sum_rails={"compute": ["cpu_big", "gpu"]}
        )
        results = processor.process_metrics(model)
        self.assertEqual(
            results,
            [
                TestCaseResult(
                    label="Power_compute",
                    unit=U.watts,
                    values=[1.5, 4.5],
                    doc="ODPM power usage samples for rails cpu_big, gpu",
                )
            ],
        )

        # Add a 5th sample to cpu_big (total 5 vs gpu's 2 -> discrepancy of 3, raises ValueError)
        thread.events.append(
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "cpu_big_odpm_rail",
                    "ts": 5000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 5000.0},
                }
            )
        )
        with self.assertRaises(ValueError):
            processor.process_metrics(model)

    def test_process_metrics_sum_rails(self) -> None:
        """Verifies sample-by-sample sum across specific lists of rails in sum_rails."""
        model = self.construct_trace_model()
        processor = odpm_power.OdpmPowerMetricsProcessor(
            sum_rails={"compute": ["cpu_big", "gpu"]}
        )
        results = processor.process_metrics(model)

        # cpu_big (1.0, 3.0) + gpu (0.5, 1.5) = (1.5, 4.5)
        expected = [
            TestCaseResult(
                label="Power_compute",
                unit=U.watts,
                values=[1.5, 4.5],
                doc="ODPM power usage samples for rails cpu_big, gpu",
            ),
        ]
        self.assertEqual(results, expected)

    def test_process_metrics_sum_rails_callable_predicate(self) -> None:
        """Verifies sum_rails accepts a callable predicate to filter rails in a single pass."""
        model = self.construct_trace_model()
        thread = model.processes[0].threads[0]
        for ts, mw in ((1000, 9000.0), (2000, 9000.0)):
            thread.events.append(
                trace_model.CounterEvent.consume_dict(
                    {
                        "cat": "power",
                        "name": "battery_odpm_rail",
                        "ts": ts,
                        "pid": 100,
                        "tid": 101,
                        "args": {"mW": mw},
                    }
                )
            )

        processor = odpm_power.OdpmPowerMetricsProcessor(
            all_rails=True,
            sum_rails={"all_rails_except_battery": lambda r: r != "battery"},
        )
        results = processor.process_metrics(model)

        # cpu_big (1.0, 3.0) + gpu (0.5, 1.5) + wlan_bt (0.25, 0.75) = (1.75, 5.25)
        expected = [
            TestCaseResult(
                label="Power_rail_battery",
                unit=U.watts,
                values=[9.0, 9.0],
                doc="ODPM power usage samples for rail battery",
            ),
            TestCaseResult(
                label="Power_rail_cpu_big",
                unit=U.watts,
                values=[1.0, 3.0],
                doc="ODPM power usage samples for rail cpu_big",
            ),
            TestCaseResult(
                label="Power_rail_gpu",
                unit=U.watts,
                values=[0.5, 1.5],
                doc="ODPM power usage samples for rail gpu",
            ),
            TestCaseResult(
                label="Power_rail_wlan_bt",
                unit=U.watts,
                values=[0.25, 0.75],
                doc="ODPM power usage samples for rail wlan_bt",
            ),
            TestCaseResult(
                label="Power_all_rails_except_battery",
                unit=U.watts,
                values=[1.75, 5.25],
                doc="ODPM power usage samples for rails cpu_big, gpu, wlan_bt",
            ),
        ]
        self.assertEqual(results, expected)

    def test_sum_rails_callable_predicate_no_match_raises_value_error(
        self,
    ) -> None:
        """Verifies that a callable predicate in sum_rails matching no rails raises ValueError."""
        model = self.construct_trace_model()
        processor = odpm_power.OdpmPowerMetricsProcessor(
            sum_rails={"nonexistent_group": lambda r: r == "nonexistent"}
        )
        with self.assertRaisesRegex(
            ValueError,
            r"No ODPM power samples matched filter for sum_rails group 'nonexistent_group'\.",
        ):
            processor.process_metrics(model)

    def test_sum_rails_missing_rail_raises_value_error(self) -> None:
        """Verifies that a missing rail in sum_rails raises ValueError."""
        model = self.construct_trace_model()
        # One present rail ("cpu_big") and one missing rail ("cpu_little")
        processor_partial = odpm_power.OdpmPowerMetricsProcessor(
            sum_rails={"cpu_total": ["cpu_big", "cpu_little"]}
        )
        with self.assertRaises(ValueError):
            processor_partial.process_metrics(model)

        # All requested rails missing from trace
        processor_all_missing = odpm_power.OdpmPowerMetricsProcessor(
            sum_rails={"cpu_total": ["cpu_little", "cpu_mid"]}
        )
        with self.assertRaises(ValueError):
            processor_all_missing.process_metrics(model)

    def test_process_metrics_rails_callable_predicate(self) -> None:
        """Verifies a rails predicate filters the rails in sorted order."""
        model = self.construct_trace_model()
        # Reverse the order of the rails in the trace, keeping sample order.
        model.processes[0].threads[0].events.sort(
            key=lambda event: event.name, reverse=True
        )
        processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=lambda r: r != "gpu"
        )
        results = processor.process_metrics(model)

        expected = [
            TestCaseResult(
                label="Power_rail_cpu_big",
                unit=U.watts,
                values=[1.0, 3.0],
                doc="ODPM power usage samples for rail cpu_big",
            ),
            TestCaseResult(
                label="Power_rail_wlan_bt",
                unit=U.watts,
                values=[0.25, 0.75],
                doc="ODPM power usage samples for rail wlan_bt",
            ),
        ]
        self.assertEqual(results, expected)

    def test_process_metrics_rails_and_sum_rails_callable_predicate(
        self,
    ) -> None:
        """Verifies one predicate can select the individual and summed rails."""
        model = self.construct_trace_model()
        thread = model.processes[0].threads[0]
        for ts in (1000, 2000):
            thread.events.append(
                trace_model.CounterEvent.consume_dict(
                    {
                        "cat": "power",
                        "name": "battery_odpm_rail",
                        "ts": ts,
                        "pid": 100,
                        "tid": 101,
                        "args": {"mW": 9000.0},
                    }
                )
            )

        non_battery_rails = lambda r: r != "battery"
        processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=non_battery_rails,
            sum_rails={"all_rails_except_battery": non_battery_rails},
        )
        results = processor.process_metrics(model)

        self.assertEqual(
            [result.label for result in results],
            [
                "Power_rail_cpu_big",
                "Power_rail_gpu",
                "Power_rail_wlan_bt",
                "Power_all_rails_except_battery",
            ],
        )

    def test_trace_import_filter_rails_callable_predicate(self) -> None:
        """Verifies a callable rails predicate imports all _odpm_rail events."""
        processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=lambda r: r != "battery"
        )
        self.assertEqual(processor.event_patterns, {r".*_odpm_rail"})
        self.assertEqual(processor.category_names, set())

    def test_rails_callable_predicate_and_all_rails_raises_value_error(
        self,
    ) -> None:
        """Verifies a rails predicate with all_rails=True raises ValueError."""
        with self.assertRaises(ValueError):
            odpm_power.OdpmPowerMetricsProcessor(
                rails=lambda r: r != "battery", all_rails=True
            )

    def test_bare_str_rails_raises_value_error(self) -> None:
        """Verifies a bare str in rails or sum_rails raises ValueError."""
        with self.assertRaises(ValueError):
            odpm_power.OdpmPowerMetricsProcessor(rails="gpu")
        with self.assertRaises(ValueError):
            odpm_power.OdpmPowerMetricsProcessor(sum_rails={"total": "cpu"})
