// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Ergonomic assertion traits and probe helpers for driver health check and alert unit tests.

use crate::testing::dut::DriverUnderTest;
use crate::{
    Driver, HealthAlert, HealthAlertLatch, HealthAlertReporter, HealthCheckResponse,
    HealthCondition, HealthDiagnosticItem, HealthDiagnosticValue, HealthStatus, overall_status,
};
use futures::channel::mpsc;

/// Lookup helpers for inspecting [`HealthDiagnosticItem`] lists in unit tests.
pub trait HealthDiagnosticsExt {
    /// Returns the [`HealthDiagnosticValue`] for the first diagnostic item matching `key`.
    fn diagnostic_value(&self, key: &str) -> Option<&HealthDiagnosticValue>;

    /// Returns the boolean value for the first diagnostic item matching `key`.
    fn diag_bool(&self, key: &str) -> Option<bool> {
        match self.diagnostic_value(key)? {
            HealthDiagnosticValue::BoolValue(v) => Some(*v),
            _ => None,
        }
    }

    /// Returns the `u64` value for the first diagnostic item matching `key`.
    fn diag_uint(&self, key: &str) -> Option<u64> {
        match self.diagnostic_value(key)? {
            HealthDiagnosticValue::UintValue(v) => Some(*v),
            _ => None,
        }
    }

    /// Returns the `i64` value for the first diagnostic item matching `key`.
    fn diag_int(&self, key: &str) -> Option<i64> {
        match self.diagnostic_value(key)? {
            HealthDiagnosticValue::IntValue(v) => Some(*v),
            _ => None,
        }
    }

    /// Returns the string slice value for the first diagnostic item matching `key`.
    fn diag_str(&self, key: &str) -> Option<&str> {
        match self.diagnostic_value(key)? {
            HealthDiagnosticValue::StringValue(v) => Some(v.as_str()),
            _ => None,
        }
    }

    /// Returns the boot-timeline timestamp (in nanoseconds) for the first diagnostic item matching
    /// `key`.
    fn diag_timestamp(&self, key: &str) -> Option<i64> {
        match self.diagnostic_value(key)? {
            HealthDiagnosticValue::Timestamp(v) => Some(*v),
            _ => None,
        }
    }
}

impl HealthDiagnosticsExt for [HealthDiagnosticItem] {
    fn diagnostic_value(&self, key: &str) -> Option<&HealthDiagnosticValue> {
        self.iter().find(|item| item.key == key).map(|item| &item.value)
    }
}

impl HealthDiagnosticsExt for HealthCondition {
    fn diagnostic_value(&self, key: &str) -> Option<&HealthDiagnosticValue> {
        self.diagnostics.as_deref()?.diagnostic_value(key)
    }
}

impl HealthDiagnosticsExt for HealthCheckResponse {
    fn diagnostic_value(&self, key: &str) -> Option<&HealthDiagnosticValue> {
        self.diagnostics.as_deref()?.diagnostic_value(key)
    }
}

/// Assertion and query helpers for [`HealthCheckResponse`] in driver unit tests.
pub trait HealthCheckResponseExt: HealthDiagnosticsExt {
    /// Derives the overall [`HealthStatus`] (`Unhealthy > Degraded > Ok`) from the response's
    /// `conditions` list, or [`HealthStatus::Ok`] when `conditions` is absent or empty.
    fn overall_status(&self) -> HealthStatus;

    /// Returns `true` when [`Self::overall_status`] is [`HealthStatus::Ok`].
    fn is_healthy(&self) -> bool {
        self.overall_status() == HealthStatus::Ok
    }

    /// Returns the first [`HealthCondition`] in `self.conditions` matching `reason_code`.
    fn condition(&self, reason_code: &str) -> Option<&HealthCondition>;

    /// Returns the [`HealthStatus`] of the first condition matching `reason_code` (normalizing an
    /// omitted status to [`HealthStatus::Unhealthy`]), or `None` if no condition has
    /// `reason_code`.
    fn condition_status(&self, reason_code: &str) -> Option<HealthStatus> {
        self.condition(reason_code).map(|c| c.status.unwrap_or(HealthStatus::Unhealthy))
    }
}

impl HealthCheckResponseExt for HealthCheckResponse {
    fn overall_status(&self) -> HealthStatus {
        self.conditions.as_deref().map_or(HealthStatus::Ok, overall_status)
    }

    fn condition(&self, reason_code: &str) -> Option<&HealthCondition> {
        self.conditions.as_deref()?.iter().find(|c| c.reason_code.as_deref() == Some(reason_code))
    }
}

/// Synchronous alert assertion helpers for test alert receivers ([`mpsc::Receiver<HealthAlert>`],
/// [`mpsc::UnboundedReceiver<HealthAlert>`], [`DriverUnderTest`], and [`TestHealthProbe`]).
///
/// These helpers only see alerts that are already queued. For [`TestHealthProbe`] and raw
/// channels every alert is queued synchronously by the reporter, so they are deterministic.
/// For [`DriverUnderTest`] an alert is queued once the harness's client task has read the
/// `OnHealthAlert` event: alerts the driver emits before replying to a FIDL call the test
/// awaited are guaranteed to be queued, but alerts emitted later by driver background tasks
/// are not. Await [`DriverUnderTest::next_health_alert`] for those before using these
/// helpers, and treat [`Self::assert_no_alerts`] as a best-effort check in that case.
pub trait HealthAlertReceiverExt {
    /// Polls for an already-queued [`HealthAlert`] without blocking.
    fn try_next_alert(&mut self) -> Option<HealthAlert>;

    /// Asserts that the next queued alert is a [`HealthAlert::SelfFault`] containing a condition
    /// with `reason_code` and `expected_status`, and returns its [`HealthCheckResponse`].
    #[track_caller]
    fn expect_self_fault(
        &mut self,
        reason_code: &str,
        expected_status: HealthStatus,
    ) -> HealthCheckResponse {
        let Some(alert) = self.try_next_alert() else {
            panic!(
                "expected SelfFault({reason_code}, {expected_status:?}), but no alert was queued"
            );
        };
        let HealthAlert::SelfFault(resp) = alert else {
            panic!("expected SelfFault({reason_code}, {expected_status:?}), got {alert:?}");
        };
        let Some(cond) = resp.condition(reason_code) else {
            panic!(
                "expected condition {reason_code:?} in SelfFault response, got conditions: {:?}",
                resp.conditions
            );
        };
        let actual_status = cond.status.unwrap_or(HealthStatus::Unhealthy);
        assert_eq!(
            actual_status, expected_status,
            "unexpected status for SelfFault condition {reason_code:?}"
        );
        resp
    }

    /// Asserts that the next queued alert is a recovery [`HealthAlert::SelfFault`] (whose overall
    /// status is [`HealthStatus::Ok`]) and returns its [`HealthCheckResponse`].
    #[track_caller]
    fn expect_self_recovery(&mut self) -> HealthCheckResponse {
        let Some(alert) = self.try_next_alert() else {
            panic!("expected SelfFault recovery alert, but no alert was queued");
        };
        let HealthAlert::SelfFault(resp) = alert else {
            panic!("expected SelfFault recovery alert, got {alert:?}");
        };
        assert!(
            resp.is_healthy(),
            "expected recovery SelfFault with Ok status, got conditions: {:?}",
            resp.conditions
        );
        resp
    }

    /// Asserts that the next queued alert is a [`HealthAlert::UpstreamNeighborFault`] containing a
    /// condition with `reason_code`, and returns its [`HealthCheckResponse`].
    #[track_caller]
    fn expect_upstream_fault(&mut self, reason_code: &str) -> HealthCheckResponse {
        let Some(alert) = self.try_next_alert() else {
            panic!("expected UpstreamNeighborFault({reason_code}), but no alert was queued");
        };
        let HealthAlert::UpstreamNeighborFault(resp) = alert else {
            panic!("expected UpstreamNeighborFault({reason_code}), got {alert:?}");
        };
        assert!(
            resp.condition(reason_code).is_some(),
            "expected condition {reason_code:?} in UpstreamNeighborFault, got: {:?}",
            resp.conditions
        );
        resp
    }

    /// Asserts that the next queued alert is a [`HealthAlert::DownstreamNeighborFault`] containing
    /// a condition with `reason_code`, and returns its [`HealthCheckResponse`].
    #[track_caller]
    fn expect_downstream_fault(&mut self, reason_code: &str) -> HealthCheckResponse {
        let Some(alert) = self.try_next_alert() else {
            panic!("expected DownstreamNeighborFault({reason_code}), but no alert was queued");
        };
        let HealthAlert::DownstreamNeighborFault(resp) = alert else {
            panic!("expected DownstreamNeighborFault({reason_code}), got {alert:?}");
        };
        assert!(
            resp.condition(reason_code).is_some(),
            "expected condition {reason_code:?} in DownstreamNeighborFault, got: {:?}",
            resp.conditions
        );
        resp
    }

    /// Asserts that no [`HealthAlert`] is currently queued.
    #[track_caller]
    fn assert_no_alerts(&mut self) {
        if let Some(alert) = self.try_next_alert() {
            panic!("expected no queued health alerts, but found: {alert:?}");
        }
    }
}

impl HealthAlertReceiverExt for mpsc::Receiver<HealthAlert> {
    fn try_next_alert(&mut self) -> Option<HealthAlert> {
        self.try_recv().ok()
    }
}

impl HealthAlertReceiverExt for mpsc::UnboundedReceiver<HealthAlert> {
    fn try_next_alert(&mut self) -> Option<HealthAlert> {
        self.try_recv().ok()
    }
}

/// Only alerts already read by the harness's client task are visible; see the trait docs.
impl<D: Driver> HealthAlertReceiverExt for DriverUnderTest<'_, D> {
    fn try_next_alert(&mut self) -> Option<HealthAlert> {
        self.try_next_health_alert()
    }
}

/// Standalone unit-test probe bundling a shared [`HealthAlertLatch`], [`HealthAlertReporter`], and
/// [`mpsc::Receiver<HealthAlert>`] backed by the same underlying channel.
pub struct TestHealthProbe {
    /// Shared self-fault latch wired to `reporter`.
    pub latch: HealthAlertLatch,
    /// Reporter for emitting self or neighbor health alerts in unit tests.
    pub reporter: HealthAlertReporter,
    /// Receiver collecting all alerts emitted by `latch` and `reporter`.
    pub alerts: mpsc::Receiver<HealthAlert>,
}

impl Default for TestHealthProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl TestHealthProbe {
    /// Creates a new [`TestHealthProbe`] with `latch` and `reporter` wired to `alerts`.
    pub fn new() -> Self {
        let (reporter, alerts) = HealthAlertReporter::new_for_testing();
        let latch = HealthAlertLatch::new(reporter.clone());
        Self { latch, reporter, alerts }
    }

    /// Polls for an already-queued [`HealthAlert`] without blocking.
    pub fn try_next_alert(&mut self) -> Option<HealthAlert> {
        self.alerts.try_next_alert()
    }

    /// Asserts that the next queued alert is a [`HealthAlert::SelfFault`] containing a condition
    /// with `reason_code` and `expected_status`, and returns its [`HealthCheckResponse`].
    #[track_caller]
    pub fn expect_self_fault(
        &mut self,
        reason_code: &str,
        expected_status: HealthStatus,
    ) -> HealthCheckResponse {
        self.alerts.expect_self_fault(reason_code, expected_status)
    }

    /// Asserts that the next queued alert is a recovery [`HealthAlert::SelfFault`] and returns its
    /// [`HealthCheckResponse`].
    #[track_caller]
    pub fn expect_self_recovery(&mut self) -> HealthCheckResponse {
        self.alerts.expect_self_recovery()
    }

    /// Asserts that the next queued alert is a [`HealthAlert::UpstreamNeighborFault`] containing a
    /// condition with `reason_code`, and returns its [`HealthCheckResponse`].
    #[track_caller]
    pub fn expect_upstream_fault(&mut self, reason_code: &str) -> HealthCheckResponse {
        self.alerts.expect_upstream_fault(reason_code)
    }

    /// Asserts that the next queued alert is a [`HealthAlert::DownstreamNeighborFault`] containing
    /// a condition with `reason_code`, and returns its [`HealthCheckResponse`].
    #[track_caller]
    pub fn expect_downstream_fault(&mut self, reason_code: &str) -> HealthCheckResponse {
        self.alerts.expect_downstream_fault(reason_code)
    }

    /// Asserts that no [`HealthAlert`] is currently queued.
    #[track_caller]
    pub fn assert_no_alerts(&mut self) {
        self.alerts.assert_no_alerts();
    }
}

impl HealthAlertReceiverExt for TestHealthProbe {
    fn try_next_alert(&mut self) -> Option<HealthAlert> {
        self.alerts.try_next_alert()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{diag_bool, diag_int, diag_str, diag_timestamp, diag_uint};

    #[fuchsia::test]
    fn response_and_condition_extensions_inspect_status_and_diagnostics() {
        let empty = HealthCheckResponse::default();
        assert_eq!(empty.overall_status(), HealthStatus::Ok);
        assert!(empty.is_healthy());
        assert!(empty.condition("MISSING").is_none());
        assert_eq!(empty.condition_status("MISSING"), None);
        assert_eq!(empty.diag_bool("b"), None);

        let resp = HealthCheckResponse {
            diagnostics: Some(vec![
                diag_str("mode", "hs-g4"),
                diag_uint("transfers", 42),
                diag_int("offset", -9),
                diag_bool("powered", true),
                diag_timestamp("last_irq", 123_456),
            ]),
            conditions: Some(vec![
                HealthCondition {
                    status: Some(HealthStatus::Degraded),
                    reason_code: Some("FIFO_WARN".to_string()),
                    diagnostics: Some(vec![diag_uint("depth", 14)]),
                    ..Default::default()
                },
                HealthCondition {
                    status: Some(HealthStatus::Unhealthy),
                    reason_code: Some("PHY_LOST".to_string()),
                    ..Default::default()
                },
            ]),
        };

        assert_eq!(resp.overall_status(), HealthStatus::Unhealthy);
        assert!(!resp.is_healthy());
        assert_eq!(resp.condition_status("FIFO_WARN"), Some(HealthStatus::Degraded));
        assert_eq!(resp.condition_status("PHY_LOST"), Some(HealthStatus::Unhealthy));
        assert_eq!(resp.condition_status("UNKNOWN_CODE"), None);

        assert_eq!(resp.diag_str("mode"), Some("hs-g4"));
        assert_eq!(resp.diag_uint("transfers"), Some(42));
        assert_eq!(resp.diag_int("offset"), Some(-9));
        assert_eq!(resp.diag_bool("powered"), Some(true));
        assert_eq!(resp.diag_timestamp("last_irq"), Some(123_456));
        assert_eq!(resp.diag_bool("transfers"), None);
        assert_eq!(resp.diag_uint("missing"), None);

        let fifo = resp.condition("FIFO_WARN").expect("FIFO_WARN condition");
        assert_eq!(fifo.diag_uint("depth"), Some(14));
        assert_eq!(fifo.diag_str("depth"), None);

        let omitted_status_resp = HealthCheckResponse {
            conditions: Some(vec![HealthCondition {
                status: None,
                reason_code: Some("OMITTED_STATUS".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(omitted_status_resp.overall_status(), HealthStatus::Unhealthy);
        assert_eq!(
            omitted_status_resp.condition_status("OMITTED_STATUS"),
            Some(HealthStatus::Unhealthy)
        );
    }

    #[fuchsia::test]
    fn test_health_probe_and_receiver_assertions() {
        let mut probe = TestHealthProbe::new();
        probe.assert_no_alerts();

        // 1. SelfFault degraded -> escalation -> recovery via shared latch.
        probe.latch.raise_degraded("BUS_RETRY", "retry #1");
        let fault = probe.expect_self_fault("BUS_RETRY", HealthStatus::Degraded);
        assert_eq!(fault.overall_status(), HealthStatus::Degraded);
        probe.assert_no_alerts();

        probe.latch.raise_unhealthy("BUS_RETRY", "bus hung");
        let escalated = probe.expect_self_fault("BUS_RETRY", HealthStatus::Unhealthy);
        assert_eq!(escalated.overall_status(), HealthStatus::Unhealthy);

        probe.latch.clear("BUS_RETRY");
        let recovered = probe.expect_self_recovery();
        assert!(recovered.is_healthy());
        probe.assert_no_alerts();

        // 2. Upstream and downstream neighbor faults via shared reporter.
        probe.reporter.report_health_alert(HealthAlert::UpstreamNeighborFault(
            HealthCheckResponse {
                conditions: Some(vec![HealthCondition {
                    status: Some(HealthStatus::Unhealthy),
                    reason_code: Some("CLOCK_PARENT_TIMEOUT".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            },
        ));
        let upstream = probe.expect_upstream_fault("CLOCK_PARENT_TIMEOUT");
        assert_eq!(
            upstream.condition_status("CLOCK_PARENT_TIMEOUT"),
            Some(HealthStatus::Unhealthy)
        );

        probe.reporter.report_health_alert(HealthAlert::DownstreamNeighborFault(
            HealthCheckResponse {
                conditions: Some(vec![HealthCondition {
                    status: Some(HealthStatus::Degraded),
                    reason_code: Some("CLIENT_QUEUE_FULL".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            },
        ));
        let downstream = probe.expect_downstream_fault("CLIENT_QUEUE_FULL");
        assert_eq!(downstream.condition_status("CLIENT_QUEUE_FULL"), Some(HealthStatus::Degraded));
        probe.assert_no_alerts();
    }
}
