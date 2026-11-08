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
use starnix_uapi::errors::Errno;
use starnix_uapi::{errno, error};
use std::collections::HashSet;
use std::sync::Arc;

/// How long to wait for a power stats provider to respond to a FIDL request before failing.
const PROVIDER_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(1);

/// Connected `fuchsia.hardware.power.stats.Service` provider instance and its reported entities.
#[derive(Debug)]
pub struct PowerStatsProvider {
    instance_name: String,
    entity_names: HashSet<String>,
    proxy: Arc<fpowerstats::DeviceSynchronousProxy>,
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

    /// Queries this provider for the current residency [`fpowerstats::Stats`] of `entity_name`.
    ///
    /// Returns `Err(EIO)` if the provider is disconnected, times out, or omits `entity_name` from
    /// its report.
    pub fn entity_stats(&self, entity_name: &str) -> Result<fpowerstats::Stats, Errno> {
        let deadline = zx::MonotonicInstant::after(PROVIDER_TIMEOUT);
        let reports = self.proxy.get_stats(deadline).map_err(|e| {
            log_warn!(
                "Failed to read power stats for {entity_name} from {}: {e}",
                self.instance_name
            );
            errno!(EIO)
        })?;
        let Some(stats) = reports.into_iter().find(|stats| stats.entity == entity_name) else {
            log_warn!(
                "Power stats provider {} no longer reports {entity_name}",
                self.instance_name
            );
            return error!(EIO);
        };
        Ok(stats)
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
    use fidl::endpoints::{ServerEnd, create_sync_proxy};
    use fuchsia_async as fasync;
    use futures::StreamExt;
    use futures::channel::{mpsc, oneshot};
    use futures::future::select;
    use starnix_sync::Mutex;
    use std::pin::pin;
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

    fn spawn_server<F: Future<Output = ()> + 'static>(serve: impl FnOnce() -> F + Send + 'static) {
        std::thread::spawn(move || fasync::LocalExecutor::default().run_singlethreaded(serve()));
    }

    fn start_test_service_dir(kernel: &Kernel) -> Arc<Simple> {
        let dir = vfs::directory::immutable::simple();
        let proxy = vfs::directory::serve_read_only(dir.clone(), vfs::ExecutionScope::new());
        watch_service_dir_for_testing::<fpowerstats::ServiceMarker>(kernel, proxy);
        dir
    }

    fn add_instance(
        dir: &Arc<Simple>,
        name: &str,
        on_open: impl Fn(zx::Channel) + Send + Sync + 'static,
    ) {
        let instance_dir = vfs::pseudo_directory! {
            "device" => vfs::service::endpoint(move |_scope, channel| {
                on_open(channel.into_zx_channel());
            }),
        };
        dir.add_entry_may_overwrite(name, instance_dir, /* overwrite = */ true)
            .expect("add service instance");
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

            add_instance(&dir, "default", move |server_end| {
                spawn_server(move || {
                    let stream =
                        ServerEnd::<fpowerstats::DeviceMarker>::new(server_end).into_stream();
                    serve_provider(
                        stream,
                        vec![stats("cpu_c2", vec![state(0, 5, 120)]), stats("lpcm_soc", vec![])],
                    )
                });
            });

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
    fn entity_stats_returns_eio_when_entity_omitted_from_stats_report() {
        let (proxy, server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
        spawn_server(move || {
            serve_provider(server.into_stream(), vec![stats("b", vec![state(0, 1, 10)])])
        });
        let provider = PowerStatsProvider::new("test", ["a", "b"], proxy);

        assert_eq!(
            provider.entity_stats("b").expect("GetStats succeeded").state_residency_data[0]
                .total_time_ms,
            10
        );
        assert_eq!(provider.entity_stats("a"), error!(EIO));
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

            add_instance(&dir, "broken_closed", |_| {});
            add_instance(&dir, "healthy", move |server_end| {
                spawn_server(move || {
                    let stream =
                        ServerEnd::<fpowerstats::DeviceMarker>::new(server_end).into_stream();
                    serve_provider(stream, vec![stats("cpu_c2", vec![state(0, 4, 80)])])
                });
            });

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

            add_instance(&dir, "first", move |server_end| {
                spawn_server(move || {
                    let stream =
                        ServerEnd::<fpowerstats::DeviceMarker>::new(server_end).into_stream();
                    serve_provider(
                        stream,
                        vec![
                            stats("shared", vec![state(0, 1, 10)]),
                            stats("only_first", vec![state(0, 2, 20)]),
                        ],
                    )
                });
            });
            let first = notified_rx.next().await.expect("first provider discovered");

            add_instance(&dir, "second", move |server_end| {
                spawn_server(move || {
                    let stream =
                        ServerEnd::<fpowerstats::DeviceMarker>::new(server_end).into_stream();
                    serve_provider(
                        stream,
                        vec![
                            stats("shared", vec![state(0, 9, 999)]),
                            stats("only_second", vec![state(0, 3, 30)]),
                        ],
                    )
                });
            });
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
            add_instance(&dir, "first", move |server_end| {
                let stop_rx = stop_rx.lock().take().expect("stop_rx taken once");
                spawn_server(move || async move {
                    let stream =
                        ServerEnd::<fpowerstats::DeviceMarker>::new(server_end).into_stream();
                    let serve_fut = pin!(serve_provider(
                        stream,
                        vec![
                            stats("cpu_c2", vec![state(0, 5, 100)]),
                            stats("unrecovered_entity", vec![state(0, 1, 10)]),
                        ],
                    ));
                    let _ = select(serve_fut, stop_rx).await;
                });
            });

            let first = announced_rx.next().await.expect("first provider discovered");
            assert_eq!(first.instance_name(), "first");
            assert_eq!(
                *first.entity_names(),
                HashSet::from(["cpu_c2".to_string(), "unrecovered_entity".to_string()])
            );
            let cpu_c2 = first.entity_stats("cpu_c2").unwrap();
            assert_eq!(cpu_c2.state_residency_data[0].total_time_ms, 100);

            // Stop `"first"`; its scope tears down and queries on the closed provider return `EIO`.
            let _ = stop_tx.send(());
            assert_eq!(disconnected_rx.next().await.as_deref(), Some("first"));
            assert_eq!(first.entity_stats("cpu_c2"), error!(EIO));
            assert_eq!(first.entity_stats("unrecovered_entity"), error!(EIO));

            // Inject a restarted instance serving `cpu_c2`; listeners receive a new provider.
            add_instance(&dir, "first_restarted", move |server_end| {
                spawn_server(move || {
                    let stream =
                        ServerEnd::<fpowerstats::DeviceMarker>::new(server_end).into_stream();
                    serve_provider(stream, vec![stats("cpu_c2", vec![state(0, 7, 250)])])
                });
            });

            let restarted = announced_rx.next().await.expect("restarted provider discovered");
            assert_eq!(restarted.instance_name(), "first_restarted");
            assert_eq!(*restarted.entity_names(), HashSet::from(["cpu_c2".to_string()]));
            let recovered = restarted.entity_stats("cpu_c2").unwrap();
            assert_eq!(recovered.state_residency_data[0].total_time_ms, 250);
            assert_eq!(restarted.entity_stats("unrecovered_entity"), error!(EIO));
        })
        .await;
    }
}
