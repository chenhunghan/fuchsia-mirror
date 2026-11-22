// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Discovery and querying of `fuchsia.hardware.power.stats.Service` provider instances.

use crate::device::service_watcher::{ServiceBindingScope, ServiceWatcher};
use crate::task::{CurrentTask, Kernel};
use fidl::endpoints::Proxy as _;
use fidl_fuchsia_hardware_power_stats as fpowerstats;
use fuchsia_async::TimeoutExt as _;
use starnix_logging::log_warn;
use starnix_sync::{LockDepMutex, PowerStatsSnapshotCacheLock};
use starnix_uapi::error;
use starnix_uapi::errors::Errno;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// How long to wait for a power stats provider to respond to a FIDL request before failing.
const PROVIDER_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(1);

/// How long to reuse a `GetStats` snapshot across back-to-back per-entity sysfs reads.
///
/// Uses the boot timeline so system suspend advances cache age alongside hardware residency
/// counters.
const STATS_CACHE_TTL: zx::BootDuration = zx::BootDuration::from_millis(50);

/// Result of one `GetStats` call.
#[derive(Debug)]
struct CachedStats {
    /// Order in which the `GetStats` call was issued.
    generation: u64,
    /// Boot instant at which the reply arrived, minus any time the system spent suspended during
    /// the call.
    fetched_at: zx::BootInstant,
    /// Stats by entity name, or `None` if the `GetStats` call failed.
    by_entity: Option<HashMap<String, fpowerstats::Stats>>,
}

/// Most recent `GetStats` result of a [`PowerStatsProvider`].
#[derive(Debug, Default)]
struct StatsCache {
    /// Generation of the most recently issued `GetStats` call.
    last_generation: u64,
    cached: Option<CachedStats>,
}

/// Clock readings that determine the age of a cached `GetStats` result.
trait CacheClock {
    /// Current boot instant.
    fn now(&self) -> zx::BootInstant;
    /// Total time the system has spent suspended since boot.
    fn suspended_time(&self) -> zx::BootDuration;
}

/// [`CacheClock`] that reads the system clocks.
struct SystemClock;

impl CacheClock for SystemClock {
    fn now(&self) -> zx::BootInstant {
        zx::BootInstant::get()
    }

    fn suspended_time(&self) -> zx::BootDuration {
        crate::power::suspended_time()
    }
}

/// Connected `fuchsia.hardware.power.stats.Service` provider instance and its reported entities.
#[derive(Debug)]
pub struct PowerStatsProvider {
    instance_name: String,
    entity_names: HashSet<String>,
    proxy: Arc<fpowerstats::DeviceSynchronousProxy>,
    cache: LockDepMutex<StatsCache, PowerStatsSnapshotCacheLock>,
}

impl PowerStatsProvider {
    /// Creates a [`PowerStatsProvider`] for `instance_name` serving `entity_names` over `proxy`.
    pub fn new(
        instance_name: impl Into<String>,
        entity_names: impl IntoIterator<Item = impl Into<String>>,
        proxy: fpowerstats::DeviceSynchronousProxy,
    ) -> Self {
        Self {
            instance_name: instance_name.into(),
            entity_names: entity_names.into_iter().map(Into::into).collect(),
            proxy: Arc::new(proxy),
            cache: LockDepMutex::new(StatsCache::default()),
        }
    }

    /// Registers `bind` to be invoked for each connected [`PowerStatsProvider`] instance, with a
    /// [`ServiceBindingScope`] tied to that provider's connection lifetime.
    ///
    /// Each `watch` call connects to and probes service instances independently, giving each caller
    /// its own [`PowerStatsProvider`] connection and [`ServiceBindingScope`].
    pub fn watch<F>(kernel: &Kernel, mut bind: F)
    where
        F: FnMut(&CurrentTask, &Arc<PowerStatsProvider>, &mut ServiceBindingScope) + Send + 'static,
    {
        ServiceWatcher::register_with_probe(
            kernel,
            fpowerstats::ServiceMarker,
            Self::probe_service_instance,
            move |current_task, provider, scope| {
                scope.monitor_channel(provider.proxy.clone());
                bind(current_task, &provider, scope);
            },
        );
    }

    /// Name of the service instance within `fuchsia.hardware.power.stats.Service`.
    pub fn instance_name(&self) -> &str {
        &self.instance_name
    }

    /// Names of the power entities reported by this provider when probed.
    pub fn entity_names(&self) -> &HashSet<String> {
        &self.entity_names
    }

    fn lookup_entity(
        &self,
        cached: &CachedStats,
        entity_name: &str,
    ) -> Result<fpowerstats::Stats, Errno> {
        let Some(by_entity) = &cached.by_entity else {
            return error!(EIO);
        };
        let Some(stats) = by_entity.get(entity_name).cloned() else {
            log_warn!(
                "Power stats provider {} no longer reports {entity_name}",
                self.instance_name
            );
            return error!(EIO);
        };
        Ok(stats)
    }

    /// Queries this provider for the current residency [`fpowerstats::Stats`] of `entity_name`.
    ///
    /// Reuses the provider's `GetStats` report for [`STATS_CACHE_TTL`] after it is received. A
    /// failed `GetStats` call is reused in the same way, so that a sweep over a hung provider
    /// waits for one [`PROVIDER_TIMEOUT`] rather than one per entity.
    ///
    /// Returns `Err(EIO)` if the provider is disconnected, times out, or omits `entity_name` from
    /// its report.
    pub fn entity_stats(&self, entity_name: &str) -> Result<fpowerstats::Stats, Errno> {
        self.entity_stats_with_clock(entity_name, &SystemClock)
    }

    fn entity_stats_with_clock(
        &self,
        entity_name: &str,
        clock: &impl CacheClock,
    ) -> Result<fpowerstats::Stats, Errno> {
        // `GetStats` runs with `cache` unlocked, so that readers never wait on the lock for a slow
        // or hung provider. Readers that find the report expired at the same time each call
        // `GetStats`.
        let generation = {
            let mut cache = self.cache.lock();
            if let Some(cached) = &cache.cached
                && clock.now() < cached.fetched_at + STATS_CACHE_TTL
            {
                return self.lookup_entity(cached, entity_name);
            }
            cache.last_generation += 1;
            cache.last_generation
        };
        let fetched = self.fetch_stats(clock, generation);
        let result = self.lookup_entity(&fetched, entity_name);
        // A reply to an earlier call does not replace the reply to a later call.
        let mut cache = self.cache.lock();
        if cache.cached.as_ref().is_none_or(|cached| cached.generation < generation) {
            cache.cached = Some(fetched);
        }
        result
    }

    /// Calls `GetStats`, timestamping the result when the call returns so that a failure after
    /// [`PROVIDER_TIMEOUT`] is still reused for [`STATS_CACHE_TTL`].
    ///
    /// The provider may read its counters before a system suspend that ends before the reply
    /// arrives, so the timestamp of a report excludes time spent suspended during the call. A
    /// failure has no counters, so its timestamp is not adjusted.
    fn fetch_stats(&self, clock: &impl CacheClock, generation: u64) -> CachedStats {
        let suspended_before = clock.suspended_time();
        let deadline = zx::MonotonicInstant::after(PROVIDER_TIMEOUT);
        let by_entity = match self.proxy.get_stats(deadline) {
            Ok(reports) => {
                Some(reports.into_iter().map(|stats| (stats.entity.clone(), stats)).collect())
            }
            Err(e) => {
                log_warn!("Failed to read power stats from {}: {e}", self.instance_name);
                None
            }
        };
        // `now` is read before `suspended_time()`, so that a suspend between the two reads makes
        // the report older rather than newer.
        let now = clock.now();
        let fetched_at = if by_entity.is_some() {
            // Clock-read jitter in `suspended_time()` can make the difference slightly negative.
            now - (clock.suspended_time() - suspended_before).max(zx::BootDuration::ZERO)
        } else {
            now
        };
        CachedStats { generation, fetched_at, by_entity }
    }

    async fn probe_service_instance(
        instance: Arc<fpowerstats::ServiceProxy>,
    ) -> Option<Arc<PowerStatsProvider>> {
        let instance_name = instance.instance_name().to_string();
        let device_proxy = match instance.connect_to_device() {
            Ok(proxy) => proxy,
            Err(e) => {
                log_warn!("Failed to connect to power stats provider {instance_name}: {e:#}");
                return None;
            }
        };
        let entities = match async { Some(device_proxy.get_entities().await) }
            .on_timeout(PROVIDER_TIMEOUT, || None)
            .await
        {
            Some(Ok(entities)) => entities,
            Some(Err(e)) => {
                log_warn!("Failed to get power entities from {instance_name}: {e:#}");
                return None;
            }
            None => {
                log_warn!("Timed out getting power entities from {instance_name}");
                return None;
            }
        };
        let client_end = match device_proxy.into_client_end() {
            Ok(client_end) => client_end,
            Err(_) => {
                log_warn!("Failed to unwrap device proxy channel for {instance_name}");
                return None;
            }
        };
        let entity_names = entities.into_iter().map(|entity| entity.name);
        let sync_proxy = fpowerstats::DeviceSynchronousProxy::new(client_end.into_channel());
        Some(Arc::new(PowerStatsProvider::new(instance_name, entity_names, sync_proxy)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::service_watcher::watch_service_dir_for_testing;
    use crate::testing::spawn_kernel_and_run;
    use fidl::endpoints::{create_sync_proxy, create_sync_proxy_and_stream};
    use fuchsia_async as fasync;
    use futures::StreamExt;
    use futures::channel::{mpsc, oneshot};
    use futures::future::{join, select};
    use starnix_sync::Mutex;
    use std::pin::pin;
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
    use vfs::directory::helper::DirectlyMutable as _;
    use vfs::directory::immutable::Simple;

    fn state(id: i32, entry_count: u32, total_time_ms: u64) -> fpowerstats::StateResidency {
        fpowerstats::StateResidency {
            id,
            name: id.to_string(),
            total_time_ms,
            entry_count,
            last_entry_timestamp_ms: 0,
        }
    }

    fn stats(
        entity: &str,
        state_residency_data: Vec<fpowerstats::StateResidency>,
    ) -> fpowerstats::Stats {
        fpowerstats::Stats { entity: entity.to_string(), state_residency_data }
    }

    async fn serve_provider(
        stream: fpowerstats::DeviceRequestStream,
        reports: Vec<fpowerstats::Stats>,
    ) {
        let _ = stream
            .for_each(|request| {
                let reports = &reports;
                async move {
                    match request {
                        Ok(fpowerstats::DeviceRequest::GetEntities { responder }) => {
                            let entities: Vec<_> = reports
                                .iter()
                                .map(|stats| fpowerstats::Entity {
                                    name: stats.entity.clone(),
                                    states: vec![],
                                })
                                .collect();
                            let _ = responder.send(&entities);
                        }
                        Ok(fpowerstats::DeviceRequest::GetStats { responder }) => {
                            let _ = responder.send(reports);
                        }
                        _ => {}
                    }
                }
            })
            .await;
    }

    fn start_test_service_dir(kernel: &Kernel) -> Arc<Simple> {
        let dir = vfs::directory::immutable::simple();
        watch_service_dir_for_testing::<fpowerstats::ServiceMarker>(kernel, dir.clone());
        dir
    }

    fn add_service_instance(dir: &Simple, name: &str, service: Arc<vfs::service::Service>) {
        let instance_dir = vfs::pseudo_directory! {
            "device" => service,
        };
        dir.add_entry_may_overwrite(name, instance_dir, /* overwrite = */ true)
            .expect("add service instance");
    }

    fn add_provider_instance(dir: &Simple, name: &str, reports: Vec<fpowerstats::Stats>) {
        add_service_instance(
            dir,
            name,
            vfs::service::host(move |stream| serve_provider(stream, reports.clone())),
        );
    }

    #[::fuchsia::test]
    async fn discovers_entities_and_queries_stats() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let dir = start_test_service_dir(kernel);

            let (notified_tx, mut notified_rx) = mpsc::unbounded();
            PowerStatsProvider::watch(kernel, move |_, provider, _| {
                let _ = notified_tx.unbounded_send(provider.clone());
            });

            add_provider_instance(
                &dir,
                "default",
                vec![stats("cpu_c2", vec![state(0, 5, 120)]), stats("lpcm_soc", vec![])],
            );

            let provider = notified_rx.next().await.expect("provider discovered");
            assert_eq!(provider.instance_name(), "default");
            assert_eq!(
                *provider.entity_names(),
                HashSet::from(["cpu_c2".to_string(), "lpcm_soc".to_string()])
            );
            let cpu_c2 = provider.entity_stats("cpu_c2").expect("GetStats succeeded");
            assert_eq!(cpu_c2.state_residency_data.len(), 1);
            assert_eq!(cpu_c2.state_residency_data[0].entry_count, 5);
            assert_eq!(cpu_c2.state_residency_data[0].total_time_ms, 120);
            assert_eq!(provider.entity_stats("unknown"), error!(EIO));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn entity_stats_returns_eio_when_entity_omitted_from_stats_report() {
        let (proxy, stream) = create_sync_proxy_and_stream::<fpowerstats::DeviceMarker>();
        join(
            serve_provider(stream, vec![stats("b", vec![state(0, 1, 10)])]),
            fasync::unblock(move || {
                let provider = PowerStatsProvider::new("test", ["a", "b"], proxy);
                assert_eq!(
                    provider.entity_stats("b").expect("GetStats succeeded").state_residency_data[0]
                        .total_time_ms,
                    10
                );
                assert_eq!(provider.entity_stats("a"), error!(EIO));
            }),
        )
        .await;
    }

    #[::fuchsia::test]
    fn entity_stats_returns_eio_when_provider_times_out() {
        // Keep `_server` open without servicing requests so `GetStats` times out.
        let (proxy, _server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
        let provider = PowerStatsProvider::new("hung", ["a"], proxy);

        assert_eq!(provider.entity_stats("a"), error!(EIO));
    }

    #[::fuchsia::test]
    async fn probe_service_instance_skips_failing_provider_and_discovers_subsequent_instances() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let dir = start_test_service_dir(kernel);

            let (notified_tx, mut notified_rx) = mpsc::unbounded();
            PowerStatsProvider::watch(kernel, move |_, provider, _| {
                let _ = notified_tx.unbounded_send(provider.clone());
            });

            add_service_instance(&dir, "broken_closed", vfs::service::endpoint(|_, _| {}));
            add_provider_instance(&dir, "healthy", vec![stats("cpu_c2", vec![state(0, 4, 80)])]);

            let provider = notified_rx.next().await.expect("healthy provider discovered");
            assert_eq!(provider.instance_name(), "healthy");
            let cpu_c2 = provider.entity_stats("cpu_c2").expect("GetStats succeeded");
            assert_eq!(cpu_c2.state_residency_data[0].total_time_ms, 80);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn providers_retain_complete_entity_names_and_query_own_instance() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let dir = start_test_service_dir(kernel);

            let (notified_tx, mut notified_rx) = mpsc::unbounded();
            PowerStatsProvider::watch(kernel, move |_, provider, _| {
                let _ = notified_tx.unbounded_send(provider.clone());
            });

            add_provider_instance(
                &dir,
                "first",
                vec![
                    stats("shared", vec![state(0, 1, 10)]),
                    stats("only_first", vec![state(0, 2, 20)]),
                ],
            );
            let first = notified_rx.next().await.expect("first provider discovered");

            add_provider_instance(
                &dir,
                "second",
                vec![
                    stats("shared", vec![state(0, 9, 999)]),
                    stats("only_second", vec![state(0, 3, 30)]),
                ],
            );
            let second = notified_rx.next().await.expect("second provider discovered");

            assert_eq!(
                *first.entity_names(),
                HashSet::from(["shared".to_string(), "only_first".to_string()])
            );
            assert_eq!(
                *second.entity_names(),
                HashSet::from(["shared".to_string(), "only_second".to_string()])
            );

            assert_eq!(
                first.entity_stats("shared").unwrap().state_residency_data[0].total_time_ms,
                10
            );
            assert_eq!(
                first.entity_stats("only_first").unwrap().state_residency_data[0].total_time_ms,
                20
            );
            assert_eq!(
                second.entity_stats("shared").unwrap().state_residency_data[0].total_time_ms,
                999
            );
            assert_eq!(
                second.entity_stats("only_second").unwrap().state_residency_data[0].total_time_ms,
                30
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn watcher_disconnect_tears_down_provider_and_restarted_instance_rebinds() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let dir = start_test_service_dir(kernel);

            let (announced_tx, mut announced_rx) = mpsc::unbounded();
            let (disconnected_tx, mut disconnected_rx) = mpsc::unbounded();
            PowerStatsProvider::watch(kernel, move |_, provider, scope| {
                let _ = announced_tx.unbounded_send(provider.clone());
                let disconnected_tx = disconnected_tx.clone();
                let instance_name = provider.instance_name().to_string();
                scope.on_disconnect(move |_| {
                    let _ = disconnected_tx.unbounded_send(instance_name);
                });
            });

            let (stop_tx, stop_rx) = oneshot::channel::<()>();
            let stop_rx = Arc::new(Mutex::new(Some(stop_rx)));
            add_service_instance(
                &dir,
                "first",
                vfs::service::host(move |stream| {
                    let stop_rx = stop_rx.lock().take().expect("stop_rx taken once");
                    async move {
                        let serve_fut = pin!(serve_provider(
                            stream,
                            vec![
                                stats("cpu_c2", vec![state(0, 5, 100)]),
                                stats("unrecovered_entity", vec![state(0, 1, 10)]),
                            ],
                        ));
                        let _ = select(serve_fut, stop_rx).await;
                    }
                }),
            );

            let first = announced_rx.next().await.expect("first provider discovered");
            assert_eq!(first.instance_name(), "first");
            assert_eq!(
                *first.entity_names(),
                HashSet::from(["cpu_c2".to_string(), "unrecovered_entity".to_string()])
            );
            let cpu_c2 = first.entity_stats("cpu_c2").unwrap();
            assert_eq!(cpu_c2.state_residency_data[0].total_time_ms, 100);

            // Stop `"first"`; its scope tears down and, once the cached report expires, queries on
            // the closed provider return `EIO`.
            let _ = stop_tx.send(());
            assert_eq!(disconnected_rx.next().await.as_deref(), Some("first"));
            let expired = FakeClock::at(zx::BootInstant::get() + STATS_CACHE_TTL);
            assert_eq!(first.entity_stats_with_clock("cpu_c2", &*expired), error!(EIO));
            assert_eq!(first.entity_stats_with_clock("unrecovered_entity", &*expired), error!(EIO));

            // Inject a restarted instance serving `cpu_c2`; listeners receive a new provider.
            add_provider_instance(
                &dir,
                "first_restarted",
                vec![stats("cpu_c2", vec![state(0, 7, 250)])],
            );

            let restarted = announced_rx.next().await.expect("restarted provider discovered");
            assert_eq!(restarted.instance_name(), "first_restarted");
            assert_eq!(*restarted.entity_names(), HashSet::from(["cpu_c2".to_string()]));
            let recovered = restarted.entity_stats("cpu_c2").unwrap();
            assert_eq!(recovered.state_residency_data[0].total_time_ms, 250);
            assert_eq!(restarted.entity_stats("unrecovered_entity"), error!(EIO));
        })
        .await;
    }

    /// [`CacheClock`] that tests set and advance by hand.
    struct FakeClock {
        now_nanos: AtomicI64,
        suspended_nanos: AtomicI64,
    }

    impl FakeClock {
        fn at(now: zx::BootInstant) -> Arc<Self> {
            Arc::new(Self {
                now_nanos: AtomicI64::new(now.into_nanos()),
                suspended_nanos: AtomicI64::new(0),
            })
        }

        fn set(&self, now: zx::BootInstant) {
            self.now_nanos.store(now.into_nanos(), Ordering::Relaxed);
        }

        fn advance(&self, duration: zx::BootDuration) {
            self.now_nanos.fetch_add(duration.into_nanos(), Ordering::Relaxed);
        }

        fn suspend(&self, duration: zx::BootDuration) {
            self.advance(duration);
            self.suspended_nanos.fetch_add(duration.into_nanos(), Ordering::Relaxed);
        }
    }

    impl CacheClock for FakeClock {
        fn now(&self) -> zx::BootInstant {
            zx::BootInstant::from_nanos(self.now_nanos.load(Ordering::Relaxed))
        }

        fn suspended_time(&self) -> zx::BootDuration {
            zx::BootDuration::from_nanos(self.suspended_nanos.load(Ordering::Relaxed))
        }
    }

    /// Serves `GetStats` on `stream`, replying to the `n`th call (counting from 1) with
    /// `reports(n)` and recording the number of calls served in `call_count`.
    async fn serve_stats(
        mut stream: fpowerstats::DeviceRequestStream,
        call_count: Arc<AtomicU64>,
        reports: impl Fn(u64) -> Vec<fpowerstats::Stats>,
    ) {
        while let Some(Ok(request)) = stream.next().await {
            if let fpowerstats::DeviceRequest::GetStats { responder } = request {
                let n = call_count.fetch_add(1, Ordering::Relaxed) + 1;
                let _ = responder.send(&reports(n));
            }
        }
    }

    fn total_time_ms(
        provider: &PowerStatsProvider,
        entity: &str,
        clock: &FakeClock,
    ) -> Result<u64, Errno> {
        Ok(provider.entity_stats_with_clock(entity, clock)?.state_residency_data[0].total_time_ms)
    }

    #[::fuchsia::test]
    async fn entity_stats_reuses_snapshot_within_ttl_and_refreshes_after_expiry() {
        let (proxy, stream) = create_sync_proxy_and_stream::<fpowerstats::DeviceMarker>();
        let call_count = Arc::new(AtomicU64::new(0));
        join(
            serve_stats(stream, call_count.clone(), |n| {
                vec![stats("a", vec![state(0, 1, n * 10)]), stats("b", vec![state(0, 2, n * 100)])]
            }),
            fasync::unblock(move || {
                let provider = PowerStatsProvider::new("cached", ["a", "b"], proxy);
                let t0 = zx::BootInstant::get();
                let clock = FakeClock::at(t0);

                assert_eq!(total_time_ms(&provider, "a", &clock), Ok(10));
                clock.advance(zx::BootDuration::from_millis(10));
                assert_eq!(total_time_ms(&provider, "b", &clock), Ok(100));
                assert_eq!(call_count.load(Ordering::Relaxed), 1);

                clock.set(t0 + STATS_CACHE_TTL);
                assert_eq!(total_time_ms(&provider, "a", &clock), Ok(20));
                assert_eq!(call_count.load(Ordering::Relaxed), 2);
            }),
        )
        .await;
    }

    #[::fuchsia::test]
    async fn entity_stats_ttl_starts_when_get_stats_reply_arrives() {
        let (proxy, stream) = create_sync_proxy_and_stream::<fpowerstats::DeviceMarker>();
        let t0 = zx::BootInstant::get();
        let clock = FakeClock::at(t0);
        let server_clock = clock.clone();
        let call_count = Arc::new(AtomicU64::new(0));
        join(
            serve_stats(stream, call_count.clone(), move |_| {
                // Each reply arrives 40 ms after its request.
                server_clock.advance(zx::BootDuration::from_millis(40));
                vec![stats("a", vec![state(0, 1, 10)])]
            }),
            fasync::unblock(move || {
                let provider = PowerStatsProvider::new("slow", ["a"], proxy);

                assert_eq!(total_time_ms(&provider, "a", &clock), Ok(10));
                assert_eq!(call_count.load(Ordering::Relaxed), 1);

                // 60 ms after the request but 20 ms after the reply, the report is still reused.
                clock.set(t0 + zx::BootDuration::from_millis(60));
                assert_eq!(total_time_ms(&provider, "a", &clock), Ok(10));
                assert_eq!(call_count.load(Ordering::Relaxed), 1);

                // 50 ms after the reply, the report has expired.
                clock.set(t0 + zx::BootDuration::from_millis(90));
                assert_eq!(total_time_ms(&provider, "a", &clock), Ok(10));
                assert_eq!(call_count.load(Ordering::Relaxed), 2);
            }),
        )
        .await;
    }

    #[::fuchsia::test]
    async fn entity_stats_excludes_suspend_during_get_stats_from_report_age() {
        let (proxy, stream) = create_sync_proxy_and_stream::<fpowerstats::DeviceMarker>();
        let clock = FakeClock::at(zx::BootInstant::get());
        let server_clock = clock.clone();
        let call_count = Arc::new(AtomicU64::new(0));
        join(
            serve_stats(stream, call_count.clone(), move |n| {
                // The system suspends after the provider reads its counters and before the reply
                // arrives.
                let reports = vec![stats("a", vec![state(0, 1, n * 10)])];
                server_clock.suspend(STATS_CACHE_TTL * 2);
                reports
            }),
            fasync::unblock(move || {
                let provider = PowerStatsProvider::new("suspended", ["a"], proxy);

                assert_eq!(total_time_ms(&provider, "a", &clock), Ok(10));
                // The report predates the suspend, so it has already expired when the reply
                // arrives.
                assert_eq!(total_time_ms(&provider, "a", &clock), Ok(20));
                assert_eq!(call_count.load(Ordering::Relaxed), 2);
            }),
        )
        .await;
    }

    #[::fuchsia::test]
    async fn entity_stats_reuses_failure_until_ttl() {
        let (proxy, mut stream) = create_sync_proxy_and_stream::<fpowerstats::DeviceMarker>();
        let clock = FakeClock::at(zx::BootInstant::get());
        let server_clock = clock.clone();
        let call_count = Arc::new(AtomicU64::new(0));
        let call_count_server = call_count.clone();
        join(
            async move {
                // Dropping a responder closes the channel, so the unanswered first call is kept
                // here.
                let mut unanswered = Vec::new();
                while let Some(Ok(request)) = stream.next().await {
                    if let fpowerstats::DeviceRequest::GetStats { responder } = request {
                        if call_count_server.fetch_add(1, Ordering::Relaxed) == 0 {
                            // The system suspends while the first call waits to time out.
                            server_clock.suspend(STATS_CACHE_TTL * 2);
                            unanswered.push(responder);
                        } else {
                            let _ = responder.send(&[stats("a", vec![state(0, 1, 10)])]);
                        }
                    }
                }
            },
            fasync::unblock(move || {
                let provider = PowerStatsProvider::new("recovering", ["a"], proxy);

                assert_eq!(total_time_ms(&provider, "a", &clock), error!(EIO));
                // The failure is reused despite the suspend, since it has no counters to age.
                let failed_at = clock.now();
                clock.advance(zx::BootDuration::from_millis(10));
                assert_eq!(total_time_ms(&provider, "a", &clock), error!(EIO));
                assert_eq!(call_count.load(Ordering::Relaxed), 1);

                clock.set(failed_at + STATS_CACHE_TTL);
                assert_eq!(total_time_ms(&provider, "a", &clock), Ok(10));
                assert_eq!(call_count.load(Ordering::Relaxed), 2);
            }),
        )
        .await;
    }

    #[::fuchsia::test]
    async fn entity_stats_slower_reply_does_not_replace_newer_report() {
        let (proxy, mut stream) = create_sync_proxy_and_stream::<fpowerstats::DeviceMarker>();
        let (first_received_tx, first_received_rx) = oneshot::channel::<()>();
        let (newer_cached_tx, newer_cached_rx) = oneshot::channel::<()>();
        let provider = Arc::new(PowerStatsProvider::new("racing", ["a"], proxy));
        let clock = FakeClock::at(zx::BootInstant::get());
        join(
            async move {
                let Some(Ok(fpowerstats::DeviceRequest::GetStats { responder: first })) =
                    stream.next().await
                else {
                    panic!("expected first GetStats");
                };
                first_received_tx.send(()).expect("send first_received");
                let Some(Ok(fpowerstats::DeviceRequest::GetStats { responder: second })) =
                    stream.next().await
                else {
                    panic!("expected second GetStats");
                };
                // Reply to the second call first, and to the first call only once the reply to the
                // second call is cached.
                let _ = second.send(&[stats("a", vec![state(0, 1, 20)])]);
                newer_cached_rx.await.expect("newer report cached");
                let _ = first.send(&[stats("a", vec![state(0, 1, 10)])]);
            },
            async move {
                let slow_reader = {
                    let provider = provider.clone();
                    let clock = clock.clone();
                    fasync::unblock(move || total_time_ms(&provider, "a", &clock))
                };
                first_received_rx.await.expect("first GetStats received");
                let fast_provider = provider.clone();
                let fast_clock = clock.clone();
                assert_eq!(
                    fasync::unblock(move || total_time_ms(&fast_provider, "a", &fast_clock)).await,
                    Ok(20)
                );
                newer_cached_tx.send(()).expect("signal newer report cached");
                assert_eq!(slow_reader.await, Ok(10));

                // The cached report is the reply to the second call.
                assert_eq!(total_time_ms(&provider, "a", &clock), Ok(20));
            },
        )
        .await;
    }

    #[::fuchsia::test]
    async fn entity_stats_reuses_report_until_ttl_after_channel_closes() {
        let (proxy, mut stream) = create_sync_proxy_and_stream::<fpowerstats::DeviceMarker>();
        let server_task = fasync::Task::local(async move {
            if let Some(Ok(fpowerstats::DeviceRequest::GetStats { responder })) =
                stream.next().await
            {
                let _ = responder.send(&[stats("a", vec![state(0, 1, 10)])]);
            }
        });
        let provider = Arc::new(PowerStatsProvider::new("closed", ["a"], proxy));
        let clock = FakeClock::at(zx::BootInstant::get());

        let first_provider = provider.clone();
        let first_clock = clock.clone();
        assert_eq!(
            fasync::unblock(move || total_time_ms(&first_provider, "a", &first_clock)).await,
            Ok(10)
        );
        server_task.await;
        assert_eq!(total_time_ms(&provider, "a", &clock), Ok(10));

        clock.advance(STATS_CACHE_TTL);
        assert_eq!(total_time_ms(&provider, "a", &clock), error!(EIO));
    }
}
