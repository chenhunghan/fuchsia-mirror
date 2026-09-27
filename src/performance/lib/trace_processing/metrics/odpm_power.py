#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""ODPM power trace metrics."""

import collections
import logging
import os
import pathlib
import statistics
from collections.abc import Iterable, Mapping, MutableSequence, Sequence

from reporting import metrics
from trace_processing import (
    trace_importing,
    trace_metrics,
    trace_model,
    trace_utils,
)

_LOGGER: logging.Logger = logging.getLogger(__name__)
_RAIL_EVENT_SUFFIX: str = "_odpm_rail"
_ALL_RAIL_EVENTS_PATTERN: str = f".*{_RAIL_EVENT_SUFFIX}"
_POWER_ARG_KEY: str = "mW"
_WATTS_PER_MILLIWATT: float = 1e-3

# Rails are polled rapidly in-sequence at the end of a poll interval. If trace
# start and stop occur during the poll sequence, we could legitimately have
# a discrepancy of up to 2 between the measurement lengths. A larger discrepancy
# indicates unexpected behavior.
_MAX_SAMPLE_COUNT_DISCREPANCY: int = 2


def _normalize_rail_name(rail: str) -> str:
    """Strips the `_odpm_rail` event suffix if present."""
    return rail.removesuffix(_RAIL_EVENT_SUFFIX)


def _rail_to_event_name(rail: str) -> str:
    """Returns the trace CounterEvent name for an ODPM rail."""
    return f"{_normalize_rail_name(rail)}{_RAIL_EVENT_SUFFIX}"


def get_available_rails(model: trace_model.Model) -> list[str]:
    """Returns a sorted list of ODPM rail names present in the trace model.

    Args:
        model: In-memory representation of a trace.

    Returns:
        Sorted list of unique rail names that have ODPM power counter events.
    """
    counter_events = trace_utils.filter_events(
        model.all_events(),
        type=trace_model.CounterEvent,
    )
    rails = {
        event.name.removesuffix(_RAIL_EVENT_SUFFIX)
        for event in counter_events
        if event.name.endswith(_RAIL_EVENT_SUFFIX)
        and _POWER_ARG_KEY in event.args
        and isinstance(event.args[_POWER_ARG_KEY], (int, float))
    }
    return sorted(rails)


class OdpmPowerMetricsProcessor(trace_metrics.MetricsProcessor):
    """Computes aggregate power consumption metrics from ODPM trace events.

    Given a trace containing ODPM power samples (CounterEvents named
    "<rail>_odpm_rail" with power readings in "mW" args), computes per-rail
    power usage metrics in Watts for the selected rails.
    """

    def __init__(
        self,
        rails: Iterable[str] = (),
        aggregates_only: bool = True,
        sum_rails: Mapping[str, Iterable[str]] | None = None,
        all_rails: bool = False,
    ) -> None:
        """Constructor.

        Args:
            rails: Iterable of ODPM rail names to report metrics for (e.g.,
                ["cpu_big", "cpu_mid", "cpu_little", "gpu"]). Can be empty if
                `all_rails` or `sum_rails` is specified.
            aggregates_only: When True, generates MinPower_<rail>,
                MeanPower_<rail>, and MaxPower_<rail> in Watts. When False,
                generates Power_<rail> with all sample values in Watts.
            sum_rails: Optional mapping from metric suffix name to an iterable
                of ODPM rail names to sum sample-by-sample across each poll
                cycle (e.g., {"cpu_total": ["cpu_big", "cpu_mid",
                "cpu_little"]}). Reports metrics with suffix `<group_name>`.
            all_rails: When True, reports metrics for all ODPM rails present in
                the trace in sorted order. Mutually exclusive with `rails`.

        Raises:
            ValueError: If none of `rails`, `all_rails`, or `sum_rails` are
                specified, if a `sum_rails` entry has an empty rail list, or if
                both `rails` and `all_rails` are specified.
        """
        # Preserve caller-specified order while deduplicating.
        self._rails: tuple[str, ...] = tuple(
            dict.fromkeys(_normalize_rail_name(r) for r in rails)
        )
        self._sum_rails: dict[str, tuple[str, ...]] = {
            name: tuple(
                dict.fromkeys(_normalize_rail_name(r) for r in rail_list)
            )
            for name, rail_list in (sum_rails or {}).items()
        }
        for name, rail_list in self._sum_rails.items():
            if not rail_list:
                raise ValueError(
                    f"sum_rails group '{name}' must specify at least one ODPM rail."
                )
        self._all_rails: bool = all_rails
        if self._rails and self._all_rails:
            raise ValueError("Cannot specify both `rails` and `all_rails`.")
        if not self._rails and not self._all_rails and not self._sum_rails:
            raise ValueError(
                "Must specify at least one ODPM rail, all_rails=True, or sum_rails to report."
            )
        self._aggregates_only: bool = aggregates_only

    @staticmethod
    def list_rails(trace_path: str | os.PathLike[str]) -> list[str]:
        """Returns a sorted list of ODPM rail names present in a trace file.

        Can be used before `process` runs to discover available rails in an
        `.fxt` or `.json` trace file.

        Args:
            trace_path: Path to an `.fxt` or `.json` trace file.

        Returns:
            Sorted list of unique rail names that have ODPM power counter events.
        """
        path = pathlib.Path(trace_path)
        if path.suffix == ".json":
            path_to_trace_json: str | os.PathLike[str] = path
        elif path.suffix == ".fxt":
            path_to_trace_json = trace_importing.convert_trace_file_to_json(
                trace_path=path,
                patterns={_ALL_RAIL_EVENTS_PATTERN},
            )
        else:
            raise ValueError(
                "Trace file must be in either .fxt or .json format"
            )

        model = trace_importing.create_model_from_file_path(path_to_trace_json)
        return get_available_rails(model)

    @property
    def event_patterns(self) -> set[str]:
        """Patterns describing the trace events needed to generate these metrics."""
        if self._all_rails:
            return {_ALL_RAIL_EVENTS_PATTERN}
        selected_rails = set(self._rails)
        for rail_list in self._sum_rails.values():
            selected_rails.update(rail_list)
        return {_rail_to_event_name(rail) for rail in selected_rails}

    def _results_for_series(
        self,
        metric_suffix: str,
        samples_w: list[float],
        target_description: str,
    ) -> list[metrics.TestCaseResult]:
        """Builds aggregate or raw series TestCaseResults for a power series."""
        if self._aggregates_only:
            return [
                metrics.TestCaseResult(
                    label=f"MinPower_{metric_suffix}",
                    unit=metrics.Unit.watts,
                    values=[min(samples_w)],
                    doc=f"ODPM power usage sampled for {target_description}, minimum",
                ),
                metrics.TestCaseResult(
                    label=f"MeanPower_{metric_suffix}",
                    unit=metrics.Unit.watts,
                    values=[statistics.mean(samples_w)],
                    doc=f"ODPM power usage sampled for {target_description}, mean",
                ),
                metrics.TestCaseResult(
                    label=f"MaxPower_{metric_suffix}",
                    unit=metrics.Unit.watts,
                    values=[max(samples_w)],
                    doc=f"ODPM power usage sampled for {target_description}, maximum",
                ),
            ]
        return [
            metrics.TestCaseResult(
                label=f"Power_{metric_suffix}",
                unit=metrics.Unit.watts,
                values=samples_w,
                doc=f"ODPM power usage samples for {target_description}",
            )
        ]

    def _sum_rail_samples(
        self,
        metric_suffix: str,
        rails_to_sum: Sequence[str],
        samples_by_rail: Mapping[str, list[float]],
    ) -> list[float]:
        """Validates sample count alignment and computes sample-by-sample sums across rails."""
        lengths = {r: len(samples_by_rail[r]) for r in rails_to_sum}
        min_len = min(lengths.values())
        max_len = max(lengths.values())
        if max_len - min_len > _MAX_SAMPLE_COUNT_DISCREPANCY:
            raise ValueError(
                f"Included ODPM rails for '{metric_suffix}' have sample counts "
                f"differing by more than {_MAX_SAMPLE_COUNT_DISCREPANCY} "
                f"(min={min_len}, max={max_len}): {lengths}"
            )
        if max_len > min_len:
            _LOGGER.warning(
                "Included ODPM rails for '%s' have different sample counts "
                "(min=%d, max=%d); truncating to %d samples: %s",
                metric_suffix,
                min_len,
                max_len,
                min_len,
                lengths,
            )

        # The `zip` will align entries across the sample vectors in order, discarding excess
        # samples at the end. Strictly speaking, it would be better to align based on timestamp
        # and discard entries that are part of a partial poll sequence. (See description of
        # `_MAX_SAMPLE_COUNT_DISCREPANCY`.) However, the extra complexity is likely not
        # worthwhile.
        return [
            sum(cycle_samples)
            for cycle_samples in zip(
                *(samples_by_rail[r] for r in rails_to_sum)
            )
        ]

    def process_metrics(
        self, model: trace_model.Model
    ) -> MutableSequence[metrics.TestCaseResult]:
        """Calculates per-rail and summed ODPM power metrics from the trace model.

        Args:
             model: In-memory representation of a system trace containing ODPM
                 power counter events.

        Returns:
            List of TestCaseResult objects for the selected rails and any
            `sum_rails` groups.
        """
        counter_events = trace_utils.filter_events(
            model.all_events(),
            type=trace_model.CounterEvent,
        )

        samples_by_rail: dict[str, list[float]] = collections.defaultdict(list)
        selected_rails_set = set(self._rails)
        for rail_list in self._sum_rails.values():
            selected_rails_set.update(rail_list)

        for event in counter_events:
            if not event.name.endswith(_RAIL_EVENT_SUFFIX):
                continue
            rail = event.name.removesuffix(_RAIL_EVENT_SUFFIX)
            if not self._all_rails and rail not in selected_rails_set:
                continue
            mw_val = event.args.get(_POWER_ARG_KEY)
            if isinstance(mw_val, (int, float)):
                samples_by_rail[rail].append(
                    float(mw_val) * _WATTS_PER_MILLIWATT
                )

        if not samples_by_rail and not self._sum_rails:
            available = [] if self._all_rails else get_available_rails(model)
            _LOGGER.warning(
                "No ODPM power events found for selected rails %s (all_rails=%s). Available rails in trace: %s",
                list(self._rails),
                self._all_rails,
                available,
            )
            return []

        results: list[metrics.TestCaseResult] = []
        rails_to_report: Iterable[str] = (
            sorted(samples_by_rail.keys()) if self._all_rails else self._rails
        )
        for rail in rails_to_report:
            samples_w = samples_by_rail.get(rail)
            if not samples_w:
                _LOGGER.warning(
                    "No ODPM power samples found for requested rail '%s'.", rail
                )
                continue

            results.extend(
                self._results_for_series(
                    metric_suffix=rail,
                    samples_w=samples_w,
                    target_description=f"rail {rail}",
                )
            )

        for group_name, group_rails in self._sum_rails.items():
            missing_rails = [
                r for r in group_rails if not samples_by_rail.get(r)
            ]
            if missing_rails:
                raise ValueError(
                    f"Missing ODPM power samples for rail(s) {missing_rails} required by "
                    f"sum_rails group '{group_name}'. Available rails in trace: "
                    f"{get_available_rails(model)}"
                )
            summed_samples_w = self._sum_rail_samples(
                metric_suffix=group_name,
                rails_to_sum=group_rails,
                samples_by_rail=samples_by_rail,
            )
            results.extend(
                self._results_for_series(
                    metric_suffix=group_name,
                    samples_w=summed_samples_w,
                    target_description=f"rails {', '.join(group_rails)}",
                )
            )

        return results
