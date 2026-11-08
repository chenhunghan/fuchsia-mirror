// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Result;
use cobalt_client::traits::AsEventCode;
use fidl_fuchsia_metrics as fmetrics;
use futures::StreamExt;
use log::warn;
use memory_metrics_registry::cobalt_registry;
use stalls::refaults::PageFaultProvider;
use stalls::{MemoryStallMetrics, StallProvider};
use std::convert::Infallible;
use zx::MonotonicInstant;

use crate::error_from_metrics_error;

/// Collect and publish to Cobalt memory stall and page refault increase rates, every hour.
pub async fn collect_stalls_forever(
    stalls_provider: impl StallProvider,
    page_fault_provider: impl PageFaultProvider,
    metric_event_logger: fmetrics::MetricEventLoggerProxy,
) -> Infallible {
    let mut last_stall = MemoryStallMetrics::default();
    let mut last_refaults = 0;

    // Wait for one hour after device start to get the first stall value. We don't use the one-hour
    // timer as we may have been started later than at boot exactly.
    fuchsia_async::Timer::new(MonotonicInstant::ZERO + zx::Duration::from_hours(1)).await;

    let mut timer = fuchsia_async::Interval::new(zx::Duration::from_hours(1));
    loop {
        let result: Result<()> = async {
            let new_stall = stalls_provider.get_stall_info()?;
            let new_refaults = page_fault_provider.get_refault_count();

            // The Cobalt metrics for stalls expect milliseconds, as defined in the Cobalt registry.
            let stall_some_event = fmetrics::MetricEvent {
                metric_id: cobalt_registry::MEMORY_STALLS_PER_HOUR_METRIC_ID,
                payload: fmetrics::MetricEventPayload::IntegerValue(i64::try_from(
                    new_stall.some.saturating_sub(last_stall.some).as_millis(),
                )?),
                event_codes: vec![
                    cobalt_registry::MemoryMetricDimensionStallType::Some.as_event_code(),
                ],
            };
            let stall_full_event = fmetrics::MetricEvent {
                metric_id: cobalt_registry::MEMORY_STALLS_PER_HOUR_METRIC_ID,
                payload: fmetrics::MetricEventPayload::IntegerValue(i64::try_from(
                    new_stall.full.saturating_sub(last_stall.full).as_millis(),
                )?),
                event_codes: vec![
                    cobalt_registry::MemoryMetricDimensionStallType::Full.as_event_code(),
                ],
            };
            // The counter is monotonic, but let's saturate to 0 if it
            // ever gets reset to avoid unsigned underflow.
            let refaults_event = fmetrics::MetricEvent {
                metric_id: cobalt_registry::MEMORY_PAGE_REFAULTS_PER_HOUR_METRIC_ID,
                payload: fmetrics::MetricEventPayload::IntegerValue(
                    new_refaults.saturating_sub(last_refaults).try_into().unwrap_or(i64::MAX),
                ),
                event_codes: vec![],
            };

            last_stall = new_stall;
            last_refaults = new_refaults;

            let events = vec![stall_some_event, stall_full_event, refaults_event];
            metric_event_logger
                .log_metric_events(&events)
                .await?
                .map_err(error_from_metrics_error)?;
            Ok(())
        }
        .await;
        if let Err(err) = result {
            warn!("Failed to collect or upload memory stall metrics: {:?}", err);
        }
        timer.next().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use fuchsia_async as fasync;
    use futures::task::Poll;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::time::Duration;

    fn get_stall_provider() -> impl StallProvider {
        #[derive(Clone)]
        struct FakeStallProvider {
            count: Arc<AtomicU32>,
        }

        impl Default for FakeStallProvider {
            fn default() -> Self {
                Self { count: Arc::new(AtomicU32::new(1)) }
            }
        }

        impl StallProvider for FakeStallProvider {
            fn get_stall_info(&self) -> Result<MemoryStallMetrics> {
                let count = self.count.fetch_add(1, Ordering::Relaxed);
                let memory_stall = MemoryStallMetrics {
                    some: Duration::from_millis((count * 10).into()),
                    full: Duration::from_millis((count * 20).into()),
                };
                Ok(memory_stall)
            }
        }

        FakeStallProvider::default()
    }

    fn get_page_fault_provider() -> impl PageFaultProvider {
        #[derive(Clone, Default)]
        struct FakePageFaultProvider {
            calls: Arc<AtomicU64>,
        }

        impl PageFaultProvider for FakePageFaultProvider {
            fn get_refault_count(&self) -> u64 {
                // Return 100 at hour 1 (delta = 100) and 350 at hour 2 (delta = 250) so that
                // the two intervals produce distinct deltas.
                match self.calls.fetch_add(1, Ordering::Relaxed) {
                    0 => 100,
                    _ => 350,
                }
            }

            fn get_fault_count(&self) -> u64 {
                0
            }
        }

        FakePageFaultProvider::default()
    }

    #[test]
    fn test_periodic_stalls_collection() -> Result<()> {
        // Setup executor.
        let mut exec = fasync::TestExecutor::new_with_fake_time();

        // Setup mock data providers.
        let data_provider = get_stall_provider();
        let page_fault_provider = get_page_fault_provider();

        // Setup test proxy to observe emitted events from the service.
        let (metric_event_logger, metric_event_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<fmetrics::MetricEventLoggerMarker>();

        // Set the time to shortly after boot
        exec.set_fake_time(
            (zx::MonotonicInstant::ZERO + zx::Duration::from_seconds(3 * 60)).into(),
        );
        // Service under test.
        let mut stalls_collector = fuchsia_async::Task::spawn(async move {
            collect_stalls_forever(data_provider, page_fault_provider, metric_event_logger).await
        });

        // Give the service the opportunity to run.
        assert!(
            exec.run_until_stalled(&mut stalls_collector).is_pending(),
            "Stalls collection service returned unexpectedly early"
        );

        // Ensure no metrics has been uploaded yet.
        let mut metric_event_request_future = metric_event_request_stream.into_future();
        assert!(
            exec.run_until_stalled(&mut metric_event_request_future).is_pending(),
            "Stalls collection service returned unexpectedly early"
        );

        // Fake the passage of time, so that collect_metrics may do a capture.
        assert!(
            exec.run_until_stalled(&mut std::pin::pin!(fasync::TestExecutor::advance_to(
                exec.now() + zx::Duration::from_seconds(60 * 60 + 10)
            )))
            .is_ready(),
            "Failed to advance time"
        );

        // Ensure we have one and only one event ready for consumption.
        let Poll::Ready((event, metric_event_request_stream)) =
            exec.run_until_stalled(&mut metric_event_request_future)
        else {
            panic!("Failed to receive metrics")
        };
        let event = event.ok_or_else(|| anyhow!("Metrics stream unexpectedly closed"))??;
        match event {
            fmetrics::MetricEventLoggerRequest::LogMetricEvents { events, responder, .. } => {
                assert_eq!(events.len(), 3);
                // Kernel metrics
                assert_eq!(
                    events[0],
                    fmetrics::MetricEvent {
                        metric_id: cobalt_registry::MEMORY_STALLS_PER_HOUR_METRIC_ID,
                        event_codes: vec![
                            cobalt_registry::MemoryMetricDimensionStallType::Some.as_event_code()
                        ],
                        payload: fmetrics::MetricEventPayload::IntegerValue(10)
                    }
                );
                assert_eq!(
                    events[1],
                    fmetrics::MetricEvent {
                        metric_id: cobalt_registry::MEMORY_STALLS_PER_HOUR_METRIC_ID,
                        event_codes: vec![
                            cobalt_registry::MemoryMetricDimensionStallType::Full.as_event_code()
                        ],
                        payload: fmetrics::MetricEventPayload::IntegerValue(20)
                    }
                );
                assert_eq!(
                    events[2],
                    fmetrics::MetricEvent {
                        metric_id: cobalt_registry::MEMORY_PAGE_REFAULTS_PER_HOUR_METRIC_ID,
                        event_codes: vec![],
                        payload: fmetrics::MetricEventPayload::IntegerValue(100)
                    }
                );
                responder.send(Ok(()))?;
            }
            _ => panic!("Unexpected metric event"),
        }

        let mut metric_event_request_future = metric_event_request_stream.into_future();

        assert!(exec.run_until_stalled(&mut metric_event_request_future).is_pending());

        // Advance to the next hour
        assert!(
            exec.run_until_stalled(&mut std::pin::pin!(fasync::TestExecutor::advance_to(
                (zx::MonotonicInstant::ZERO + zx::Duration::from_seconds(60 * 60 * 2 + 10)).into()
            )))
            .is_ready(),
            "Failed to advance time"
        );

        // Ensure we have one and only one event ready for consumption.
        let Poll::Ready((event, metric_event_request_stream)) =
            exec.run_until_stalled(&mut metric_event_request_future)
        else {
            panic!("Failed to receive metrics")
        };
        let event = event.ok_or_else(|| anyhow!("Metrics stream unexpectedly closed"))??;
        match event {
            fmetrics::MetricEventLoggerRequest::LogMetricEvents { events, responder, .. } => {
                assert_eq!(events.len(), 3);
                // Kernel metrics
                assert_eq!(
                    events[0],
                    fmetrics::MetricEvent {
                        metric_id: cobalt_registry::MEMORY_STALLS_PER_HOUR_METRIC_ID,
                        event_codes: vec![
                            cobalt_registry::MemoryMetricDimensionStallType::Some.as_event_code()
                        ],
                        payload: fmetrics::MetricEventPayload::IntegerValue(10)
                    }
                );
                assert_eq!(
                    events[1],
                    fmetrics::MetricEvent {
                        metric_id: cobalt_registry::MEMORY_STALLS_PER_HOUR_METRIC_ID,
                        event_codes: vec![
                            cobalt_registry::MemoryMetricDimensionStallType::Full.as_event_code()
                        ],
                        payload: fmetrics::MetricEventPayload::IntegerValue(20)
                    }
                );
                assert_eq!(
                    events[2],
                    fmetrics::MetricEvent {
                        metric_id: cobalt_registry::MEMORY_PAGE_REFAULTS_PER_HOUR_METRIC_ID,
                        event_codes: vec![],
                        payload: fmetrics::MetricEventPayload::IntegerValue(250)
                    }
                );
                responder.send(Ok(()))?;
            }
            _ => panic!("Unexpected metric event"),
        }

        assert!(
            exec.run_until_stalled(&mut metric_event_request_stream.into_future()).is_pending()
        );

        Ok(())
    }

    #[test]
    fn test_stalls_collection_continues_on_cobalt_error() -> Result<()> {
        #[derive(Clone, Default)]
        struct FlakyStallProvider {
            attempts: Arc<AtomicU32>,
        }

        impl StallProvider for FlakyStallProvider {
            fn get_stall_info(&self) -> Result<MemoryStallMetrics> {
                match self.attempts.fetch_add(1, Ordering::Relaxed) {
                    0 => Err(anyhow!("transient stall read failure")),
                    n => Ok(MemoryStallMetrics {
                        some: Duration::from_millis((n * 10).into()),
                        full: Duration::from_millis((n * 20).into()),
                    }),
                }
            }
        }

        let mut exec = fasync::TestExecutor::new_with_fake_time();
        let data_provider = FlakyStallProvider::default();
        let page_fault_provider = get_page_fault_provider();
        let (metric_event_logger, metric_event_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<fmetrics::MetricEventLoggerMarker>();

        exec.set_fake_time(
            (zx::MonotonicInstant::ZERO + zx::Duration::from_seconds(3 * 60)).into(),
        );
        let mut stalls_collector = fuchsia_async::Task::spawn(async move {
            collect_stalls_forever(data_provider, page_fault_provider, metric_event_logger).await
        });
        assert!(exec.run_until_stalled(&mut stalls_collector).is_pending());

        // Hour 1 encounters a `get_stall_info()` error: collector logs a warning and stays alive.
        assert!(
            exec.run_until_stalled(&mut std::pin::pin!(fasync::TestExecutor::advance_to(
                exec.now() + zx::Duration::from_seconds(60 * 60 + 10)
            )))
            .is_ready()
        );
        assert!(
            exec.run_until_stalled(&mut stalls_collector).is_pending(),
            "Stalls collector terminated on get_stall_info() error"
        );
        let mut metric_event_request_future = metric_event_request_stream.into_future();
        assert!(exec.run_until_stalled(&mut metric_event_request_future).is_pending());

        // Hour 2 succeeds in `get_stall_info()` and encounters `BufferFull` from Cobalt: collector
        // stays alive.
        assert!(
            exec.run_until_stalled(&mut std::pin::pin!(fasync::TestExecutor::advance_to(
                (zx::MonotonicInstant::ZERO + zx::Duration::from_seconds(60 * 60 * 2 + 10)).into()
            )))
            .is_ready()
        );
        let Poll::Ready((Some(Ok(event)), metric_event_request_stream)) =
            exec.run_until_stalled(&mut metric_event_request_future)
        else {
            panic!("Expected hour 2 LogMetricEvents request");
        };
        let fmetrics::MetricEventLoggerRequest::LogMetricEvents { responder, .. } = event else {
            panic!("Unexpected metric event");
        };
        responder.send(Err(fmetrics::Error::BufferFull))?;
        assert!(
            exec.run_until_stalled(&mut stalls_collector).is_pending(),
            "Stalls collector terminated on Cobalt BufferFull error"
        );

        // Advance past hour 3: collector still emits the next hourly stalls batch with 1-hour
        // deltas relative to hour 2.
        assert!(
            exec.run_until_stalled(&mut std::pin::pin!(fasync::TestExecutor::advance_to(
                (zx::MonotonicInstant::ZERO + zx::Duration::from_seconds(60 * 60 * 3 + 10)).into()
            )))
            .is_ready()
        );
        let mut metric_event_request_future = metric_event_request_stream.into_future();
        let Poll::Ready((Some(Ok(event)), _metric_event_request_stream)) =
            exec.run_until_stalled(&mut metric_event_request_future)
        else {
            panic!("Expected hour 3 LogMetricEvents request");
        };
        let fmetrics::MetricEventLoggerRequest::LogMetricEvents { events, responder, .. } = event
        else {
            panic!("Unexpected metric event");
        };
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].payload, fmetrics::MetricEventPayload::IntegerValue(10));
        assert_eq!(events[1].payload, fmetrics::MetricEventPayload::IntegerValue(20));
        assert_eq!(events[2].payload, fmetrics::MetricEventPayload::IntegerValue(250));
        responder.send(Ok(()))?;
        assert!(exec.run_until_stalled(&mut stalls_collector).is_pending());

        Ok(())
    }
}
