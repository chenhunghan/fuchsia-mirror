// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Scenic `FrameScheduler` frame counter and FPS telemetry via
//! `fuchsia.diagnostics.ArchiveAccessor`.

use diagnostics_hierarchy::{DiagnosticsHierarchy, Property, SelectResult, select_from_hierarchy};
use diagnostics_reader::{ArchiveReader, Inspect, RetryConfig};
use fidl::endpoints::Proxy;
use fidl_fuchsia_diagnostics as fdiagnostics;
use fuchsia_component::client::connect_to_protocol;
use std::fmt;

/// Default Inspect selector for Scenic's `DefaultFrameScheduler` frame counter (`frame_number_`).
pub const DEFAULT_SCENIC_FRAME_NUMBER_SELECTOR: &str =
    "core/ui/scenic:root/scenic/FrameScheduler:most_recent_frame_number";

const SCENIC_PROPERTY_PATH: [&str; 3] = ["scenic", "FrameScheduler", "most_recent_frame_number"];
const SCENIC_INSPECT_TIMEOUT_MS: i64 = 250;
const SCENIC_BATCH_RETRIEVAL_TIMEOUT_SECS: i64 = 1;
const MIN_SCENIC_FPS_WINDOW_NS: i64 = 100_000_000;
const MAX_SCENIC_FPS_WINDOW_NS: i64 = 2_500_000_000;
const INITIAL_EMPTY_BACKOFF_NS: i64 = 1_000_000_000;
const MAX_EMPTY_BACKOFF_NS: i64 = 60_000_000_000;
const NANOSECONDS_PER_SECOND_F64: f64 = 1_000_000_000.0;

/// Snapshot of Scenic's rendered frame counter and recent frame rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ScenicMetrics {
    /// Cumulative Scenic `FrameScheduler` frame number.
    pub frame_count: Option<u64>,
    /// Rounded frames per second over the latest `0.1s..=2.5s` sample window.
    pub fps: Option<u64>,
}

pub(crate) enum PropertySelectorKind {
    DefaultScenicPath,
    ExactPath { root: String, children: Box<[String]>, property: String },
    Wildcard(fdiagnostics::Selector),
    Invalid,
}

impl PropertySelectorKind {
    pub(crate) fn from_parsed(selector_str: &str, parsed: &fdiagnostics::Selector) -> Self {
        if selector_str == DEFAULT_SCENIC_FRAME_NUMBER_SELECTOR {
            return Self::DefaultScenicPath;
        }
        if let Some((root, children, property)) = extract_exact_property_path(parsed) {
            return Self::ExactPath { root, children, property };
        }
        Self::Wildcard(parsed.clone())
    }
}

fn extract_exact_property_path(
    selector: &fdiagnostics::Selector,
) -> Option<(String, Box<[String]>, String)> {
    let Some(fdiagnostics::TreeSelector::PropertySelector(prop_selector)) = &selector.tree_selector
    else {
        return None;
    };
    let mut node_iter = prop_selector.node_path.iter();
    let fdiagnostics::StringSelector::ExactMatch(root) = node_iter.next()? else {
        return None;
    };
    let mut children = Vec::with_capacity(prop_selector.node_path.len().saturating_sub(1));
    for segment in node_iter {
        let fdiagnostics::StringSelector::ExactMatch(name) = segment else {
            return None;
        };
        children.push(name.clone());
    }
    let fdiagnostics::StringSelector::ExactMatch(property) = &prop_selector.target_properties
    else {
        return None;
    };
    Some((root.clone(), children.into_boxed_slice(), property.clone()))
}

/// Queries Scenic's `FrameScheduler` Inspect counter using a pre-parsed structured selector and
/// computes frames per second across consecutive samples.
///
/// Note: Each Inspect snapshot evaluates Scenic's lazy Inspect callbacks on Scenic's main thread
/// via `ArchiveAccessor`. Callers should poll at a low cadence (e.g., `<= 1 Hz`); calls arriving
/// within `100ms` of the previous snapshot reuse the most recent frame counter without issuing IPC,
/// and missing payloads back off exponentially (`1s..=60s`).
pub struct ScenicCollector {
    archive_accessor: Option<fdiagnostics::ArchiveAccessorProxy>,
    reader: Option<ArchiveReader<Inspect>>,
    selector_kind: PropertySelectorKind,
    last_ipc_monotonic_ns: Option<i64>,
    last_frame_count: Option<u64>,
    next_retry_ns: Option<i64>,
    empty_backoff_ns: i64,
    previous_sample: Option<(i64, u64)>,
    last_fps: Option<u64>,
}

impl fmt::Debug for ScenicCollector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScenicCollector")
            .field("has_reader", &self.reader.is_some())
            .field("previous_sample", &self.previous_sample)
            .field("last_fps", &self.last_fps)
            .finish()
    }
}

impl ScenicCollector {
    /// Connects to `fuchsia.diagnostics.ArchiveAccessor` using
    /// [`DEFAULT_SCENIC_FRAME_NUMBER_SELECTOR`].
    pub fn from_namespace() -> Self {
        Self::with_archive_accessor(
            connect_to_protocol::<fdiagnostics::ArchiveAccessorMarker>().ok(),
        )
    }

    /// Creates an inert collector with no `ArchiveAccessor` connection.
    pub fn disconnected() -> Self {
        Self::with_archive_accessor(None)
    }

    #[cfg(test)]
    pub(crate) fn is_enabled(&self) -> bool {
        self.reader.is_some()
    }

    /// Creates a collector backed by an explicit `ArchiveAccessorProxy` using
    /// [`DEFAULT_SCENIC_FRAME_NUMBER_SELECTOR`].
    pub fn with_archive_accessor(
        archive_accessor: Option<fdiagnostics::ArchiveAccessorProxy>,
    ) -> Self {
        Self::with_archive_accessor_and_selector(
            archive_accessor,
            DEFAULT_SCENIC_FRAME_NUMBER_SELECTOR,
        )
    }

    /// Creates a collector backed by an explicit `ArchiveAccessorProxy` and custom Inspect
    /// selector, pre-parsing the selector once into a structured `fdiagnostics::Selector`.
    pub fn with_archive_accessor_and_selector(
        archive_accessor: Option<fdiagnostics::ArchiveAccessorProxy>,
        selector: impl AsRef<str>,
    ) -> Self {
        let selector_str = selector.as_ref();
        let (parsed_for_reader, selector_kind) = match selectors::parse_verbose(selector_str) {
            Ok(parsed) => {
                let kind = PropertySelectorKind::from_parsed(selector_str, &parsed);
                (Some(parsed), kind)
            }
            Err(_) => (None, PropertySelectorKind::Invalid),
        };

        let reader = match (&archive_accessor, parsed_for_reader) {
            (Some(accessor), Some(parsed)) => {
                let mut reader = ArchiveReader::inspect();
                reader
                    .with_archive(accessor.clone())
                    .add_selector(parsed)
                    .retry(RetryConfig::never())
                    .with_batch_retrieval_timeout_seconds(SCENIC_BATCH_RETRIEVAL_TIMEOUT_SECS)
                    .with_timeout(zx::MonotonicDuration::from_millis(SCENIC_INSPECT_TIMEOUT_MS));
                Some(reader)
            }
            _ => None,
        };

        Self {
            archive_accessor: if reader.is_some() { archive_accessor } else { None },
            reader,
            selector_kind,
            last_ipc_monotonic_ns: None,
            last_frame_count: None,
            next_retry_ns: None,
            empty_backoff_ns: INITIAL_EMPTY_BACKOFF_NS,
            previous_sample: None,
            last_fps: None,
        }
    }

    fn handle_failed_or_empty_snapshot(&mut self, gate_now_ns: i64) {
        if self.archive_accessor.as_ref().is_some_and(|proxy| proxy.is_closed()) {
            self.archive_accessor = None;
            self.reader = None;
        } else {
            self.next_retry_ns = Some(gate_now_ns.saturating_add(self.empty_backoff_ns));
            self.empty_backoff_ns =
                self.empty_backoff_ns.saturating_mul(2).min(MAX_EMPTY_BACKOFF_NS);
        }
    }

    async fn query_frame_number(
        &mut self,
        gate_now_ns: i64,
        use_live_post_clock: bool,
    ) -> Option<(i64, u64)> {
        self.reader.as_ref()?;
        if let Some(last_ns) = self.last_ipc_monotonic_ns
            && gate_now_ns.saturating_sub(last_ns) < MIN_SCENIC_FPS_WINDOW_NS
        {
            return self.last_frame_count.map(|frame| (last_ns, frame));
        }
        if self.next_retry_ns.is_some_and(|next_ns| gate_now_ns < next_ns) {
            return None;
        }

        let snapshot = match self.reader.as_ref()?.snapshot().await {
            Ok(data) => data,
            Err(_) => {
                self.handle_failed_or_empty_snapshot(gate_now_ns);
                return None;
            }
        };

        for data in snapshot {
            let Some(hierarchy) = data.payload else {
                continue;
            };
            if let Some(frame_number) =
                extract_frame_number_with_selector(&hierarchy, &self.selector_kind)
            {
                let stamped_ns = if use_live_post_clock {
                    zx::MonotonicInstant::get().into_nanos().max(0)
                } else {
                    gate_now_ns
                };
                self.last_ipc_monotonic_ns = Some(stamped_ns);
                self.last_frame_count = Some(frame_number);
                self.next_retry_ns = None;
                self.empty_backoff_ns = INITIAL_EMPTY_BACKOFF_NS;
                return Some((stamped_ns, frame_number));
            }
        }

        self.handle_failed_or_empty_snapshot(gate_now_ns);
        None
    }

    /// Captures a baseline `(monotonic, frame_number)` sample, reading `zx::MonotonicInstant`
    /// immediately after the Inspect snapshot completes.
    pub async fn prime_baseline(&mut self) {
        if self.reader.is_none() {
            return;
        }
        let now_ns = zx::MonotonicInstant::get().into_nanos().max(0);
        if let Some((stamped_ns, frame_number)) = self.query_frame_number(now_ns, true).await {
            self.previous_sample = Some((stamped_ns, frame_number));
        }
    }

    /// Captures a baseline `(monotonic, frame_number)` sample stamped at `monotonic`.
    pub async fn prime_baseline_at(&mut self, monotonic: zx::MonotonicInstant) {
        if self.reader.is_none() {
            return;
        }
        let now_ns = monotonic.into_nanos().max(0);
        if let Some((stamped_ns, frame_number)) = self.query_frame_number(now_ns, false).await {
            self.previous_sample = Some((stamped_ns, frame_number));
        }
    }

    /// Collects [`ScenicMetrics`], sampling `zx::MonotonicInstant` immediately after the Inspect
    /// snapshot completes.
    pub async fn collect(&mut self) -> ScenicMetrics {
        if self.reader.is_none() {
            return ScenicMetrics::default();
        }
        let now_ns = zx::MonotonicInstant::get().into_nanos().max(0);
        let Some((stamped_ns, current_frame)) = self.query_frame_number(now_ns, true).await else {
            return ScenicMetrics::default();
        };
        self.record_frame_sample(stamped_ns, current_frame)
    }

    /// Collects [`ScenicMetrics`] stamped at `monotonic`.
    pub async fn collect_at(&mut self, monotonic: zx::MonotonicInstant) -> ScenicMetrics {
        if self.reader.is_none() {
            return ScenicMetrics::default();
        }
        let now_ns = monotonic.into_nanos().max(0);
        let Some((stamped_ns, current_frame)) = self.query_frame_number(now_ns, false).await else {
            return ScenicMetrics::default();
        };
        self.record_frame_sample(stamped_ns, current_frame)
    }

    pub(crate) fn record_frame_sample(
        &mut self,
        monotonic_ns: i64,
        current_frame: u64,
    ) -> ScenicMetrics {
        let clamped_mono_ns = monotonic_ns.max(0);
        if let Some((previous_monotonic_ns, previous_frame)) = self.previous_sample {
            let delta_ns = clamped_mono_ns - previous_monotonic_ns;
            if delta_ns < MIN_SCENIC_FPS_WINDOW_NS {
                // Sub-100ms or regressed window: only preserve previous_sample unless Scenic's
                // frame counter reset (e.g. component restart).
                if current_frame < previous_frame && delta_ns >= 0 {
                    self.previous_sample = Some((clamped_mono_ns, current_frame));
                    self.last_fps = None;
                    return ScenicMetrics { frame_count: Some(current_frame), fps: None };
                }
                return ScenicMetrics { frame_count: Some(current_frame), fps: self.last_fps };
            }

            let fps = if delta_ns <= MAX_SCENIC_FPS_WINDOW_NS {
                current_frame.checked_sub(previous_frame).map(|frame_delta| {
                    ((frame_delta as f64 * NANOSECONDS_PER_SECOND_F64) / delta_ns as f64).round()
                        as u64
                })
            } else {
                None
            };

            self.previous_sample = Some((clamped_mono_ns, current_frame));
            self.last_fps = fps;
            return ScenicMetrics { frame_count: Some(current_frame), fps };
        }

        self.previous_sample = Some((clamped_mono_ns, current_frame));
        self.last_fps = None;
        ScenicMetrics { frame_count: Some(current_frame), fps: None }
    }
}

fn property_to_u64(property: &Property) -> Option<u64> {
    match property {
        Property::Uint(_, value) => Some(*value),
        Property::Int(_, value) => u64::try_from(*value).ok(),
        _ => None,
    }
}

pub(crate) fn extract_frame_number_with_selector(
    hierarchy: &DiagnosticsHierarchy,
    selector_kind: &PropertySelectorKind,
) -> Option<u64> {
    match selector_kind {
        PropertySelectorKind::DefaultScenicPath => {
            if hierarchy.name != "root" {
                return None;
            }
            hierarchy.get_property_by_path(&SCENIC_PROPERTY_PATH).and_then(property_to_u64)
        }
        PropertySelectorKind::ExactPath { root, children, property } => {
            if &hierarchy.name != root {
                return None;
            }
            let mut node = hierarchy;
            for child_name in children {
                node = node.get_child(child_name)?;
            }
            node.get_property(property).and_then(property_to_u64)
        }
        PropertySelectorKind::Wildcard(selector) => {
            let SelectResult::Properties(properties) =
                select_from_hierarchy(hierarchy, selector).ok()?
            else {
                return None;
            };
            properties.iter().find_map(|prop| property_to_u64(prop.as_ref()))
        }
        PropertySelectorKind::Invalid => None,
    }
}
