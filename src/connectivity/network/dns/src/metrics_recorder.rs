// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fuchsia_inspect::Node;
use fuchsia_sync::Mutex;
use hickory_resolver::metrics::NAME_SERVER_QUERY_RESPONSES;
use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};

/// Holds the query response counters for a single `(address, protocol)` name server entry.
///
/// Each counter is stored as an [`Arc<AtomicU64>`] because [`Recorder::register_counter`] returns
/// a [`Counter`] constructed via [`Counter::from_arc`]. Caching the [`Arc`] per `(address,
/// protocol, status)` avoids allocating a new handle on every query.
#[derive(Debug)]
struct NameServerCounters {
    /// Stable 0-based index assigned when this `(address, protocol)` entry is first
    /// registered, used as the Inspect child node name (`"0"`, `"1"`, ...).
    index: usize,
    /// Counter for `"status" => "success"`, backed by an [`AtomicU64`].
    successful_queries: Arc<AtomicU64>,
    /// Keyed by the error `"status"` label (e.g., `"timeout"`). Caches counters so repeated
    /// updates modify the existing counter in place.
    error_counters: HashMap<String, Arc<AtomicU64>>,
}

impl NameServerCounters {
    fn new(index: usize) -> Self {
        Self {
            index,
            successful_queries: Arc::new(AtomicU64::new(0)),
            error_counters: HashMap::new(),
        }
    }

    fn counter_for_status(&mut self, status: &str) -> Counter {
        if status == "success" {
            return Counter::from_arc(Arc::clone(&self.successful_queries));
        }
        if let Some(counter) = self.error_counters.get(status) {
            return Counter::from_arc(Arc::clone(counter));
        }
        let counter = Arc::new(AtomicU64::new(0));
        let _: Option<_> = self.error_counters.insert(status.to_string(), Arc::clone(&counter));
        Counter::from_arc(counter)
    }
}

/// A [`metrics::Recorder`] that collects `hickory-resolver` per-name-server query response
/// counters and records them into a Fuchsia Inspect tree on demand.
///
/// Note: Because `metrics::Recorder` is installed process-wide and `metrics` counters do not have
/// a deregistration lifecycle, per-nameserver counters are retained for the lifetime of the
/// component across DNS configuration updates.
#[derive(Debug, Default)]
pub struct InspectMetricsRecorder {
    /// Keyed first by the name server IP address string from the `"addr"` label (e.g., `"8.8.8.8"`)
    /// and then by the `"protocol"` label (e.g., `"udp"`).
    nodes: Mutex<HashMap<String, HashMap<String, NameServerCounters>>>,
}

impl InspectMetricsRecorder {
    /// Creates a new [`InspectMetricsRecorder`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Populates `node` with the current per-name-server query response metrics.
    pub fn record(&self, node: &Node) {
        let nodes = self.nodes.lock();
        for (addr, by_proto) in &*nodes {
            for (protocol, counters) in by_proto {
                let NameServerCounters { index, successful_queries, error_counters } = counters;
                node.record_child(index.to_string(), |child| {
                    child.record_string("address", addr);
                    child.record_string("protocol", protocol);
                    child.record_uint(
                        "successful_queries",
                        successful_queries.load(Ordering::Relaxed),
                    );
                    let mut failed_queries = 0u64;
                    child.record_child("errors", |errors| {
                        for (status, count) in error_counters {
                            let count = count.load(Ordering::Relaxed);
                            failed_queries += count;
                            errors.record_uint(status, count);
                        }
                    });
                    child.record_uint("failed_queries", failed_queries);
                });
            }
        }
    }
}

impl Recorder for InspectMetricsRecorder {
    fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> Counter {
        if key.name() != NAME_SERVER_QUERY_RESPONSES {
            return Counter::noop();
        }

        let mut addr = None;
        let mut protocol = None;
        let mut status = None;
        for label in key.labels() {
            match label.key() {
                "addr" => addr = Some(label.value()),
                "protocol" => protocol = Some(label.value()),
                "status" => status = Some(label.value()),
                _ => {}
            }
        }
        let addr = addr.unwrap_or("unknown");
        let protocol = protocol.unwrap_or("unknown");
        let status = status.unwrap_or("unknown");

        let mut nodes = self.nodes.lock();
        if let Some(counters) = nodes.get_mut(addr).and_then(|by_proto| by_proto.get_mut(protocol))
        {
            return counters.counter_for_status(status);
        }

        let next_index = nodes.values().map(HashMap::len).sum();
        let mut counters = NameServerCounters::new(next_index);
        let counter = counters.counter_for_status(status);
        let _: Option<_> =
            nodes.entry(addr.to_string()).or_default().insert(protocol.to_string(), counters);
        counter
    }

    fn register_gauge(&self, _key: &Key, _metadata: &Metadata<'_>) -> Gauge {
        Gauge::noop()
    }

    fn register_histogram(&self, _key: &Key, _metadata: &Metadata<'_>) -> Histogram {
        Histogram::noop()
    }

    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    use diagnostics_assertions::assert_data_tree;
    use fuchsia_inspect::Inspector;
    use hickory_resolver::metrics::OUTGOING_QUERIES_TOTAL;

    #[fuchsia::test]
    async fn inspect_metrics_recorder_name_server_responses() {
        let inspector = Inspector::default();
        let recorder = InspectMetricsRecorder::new();

        metrics::with_local_recorder(&recorder, || {
            metrics::counter!(
                NAME_SERVER_QUERY_RESPONSES,
                "addr" => "8.8.8.8",
                "protocol" => "udp",
                "status" => "success",
            )
            .increment(1);
            metrics::counter!(
                NAME_SERVER_QUERY_RESPONSES,
                "addr" => "8.8.8.8",
                "protocol" => "udp",
                "status" => "success",
            )
            .increment(1);
            metrics::counter!(
                NAME_SERVER_QUERY_RESPONSES,
                "addr" => "8.8.8.8",
                "protocol" => "udp",
                "status" => "timeout",
            )
            .increment(1);
            metrics::counter!(
                NAME_SERVER_QUERY_RESPONSES,
                "addr" => "8.8.8.8",
                "protocol" => "tcp",
                "status" => "success",
            )
            .increment(1);
            // Counters other than NAME_SERVER_QUERY_RESPONSES should be ignored and not
            // recorded in the Inspect tree.
            metrics::counter!(OUTGOING_QUERIES_TOTAL, "protocol" => "udp").increment(5);
        });

        inspector.root().record_child("name_servers", |node| recorder.record(node));

        assert_data_tree!(inspector, root: {
            name_servers: {
                "0": {
                    address: "8.8.8.8",
                    protocol: "udp",
                    successful_queries: 2u64,
                    failed_queries: 1u64,
                    errors: {
                        timeout: 1u64,
                    },
                },
                "1": {
                    address: "8.8.8.8",
                    protocol: "tcp",
                    successful_queries: 1u64,
                    failed_queries: 0u64,
                    errors: {},
                },
            }
        });
    }
}
