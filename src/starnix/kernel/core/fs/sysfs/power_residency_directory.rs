// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fs::sysfs::build_device_directory;
use crate::task::{CurrentTask, Kernel};
use crate::vfs::pseudo::simple_file::{BytesFile, BytesFileOps};
use anyhow::Context as _;
use fidl_fuchsia_hardware_power_stats as fpowerstats;
use fuchsia_component::client as fclient;
use futures::TryStreamExt;
use starnix_logging::{log_info, log_warn};
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::{errno, error};
use std::borrow::Cow;
use std::fmt::Write as _;
use std::sync::{Arc, Weak};

/// Power entities whose names start with this prefix are served as files of the same name.
const RESIDENCY_FILE_PREFIX: &str = "lpcm_";

/// Name of the platform device under which the residency files are served.
const RESIDENCY_DEVICE_NAME: &str = "powerdashboard";

/// Serves one file per power entity reported over `fuchsia.hardware.power.stats.Service` whose
/// name starts with [`RESIDENCY_FILE_PREFIX`], under the [`RESIDENCY_DEVICE_NAME`] platform
/// device.
///
/// The entities are only known once a provider has started, so the device is added in the
/// background, once a provider reports at least one such entity.
pub fn init_power_residency_device(kernel: &Kernel) {
    let weak_kernel = kernel.weak_self.clone();
    kernel.kthreads.spawn_future(
        move || async move {
            let result = match fclient::Service::open(fpowerstats::ServiceMarker) {
                Ok(service) => add_residency_device(weak_kernel, service).await,
                Err(e) => Err(e.context("failed to open power stats service directory")),
            };
            if let Err(e) = result {
                log_info!("Power residency files unavailable: {e:#}");
            }
        },
        "power_residency",
    );
}

/// Watches `service` for power stats providers and adds the device, with a file for each prefixed
/// entity, for the first provider that reports any. Prefixed entities from other providers are
/// ignored.
async fn add_residency_device(
    kernel: Weak<Kernel>,
    service: fclient::Service<fpowerstats::ServiceMarker>,
) -> Result<(), anyhow::Error> {
    let mut instances = service.watch().await.context("failed to watch power stats service")?;
    let mut device_added = false;
    while let Some(instance) = instances.try_next().await? {
        let name = instance.instance_name().to_string();
        let entities = match instance.connect_to_device() {
            Ok(device) => device.get_entities().await.map_err(anyhow::Error::from),
            Err(e) => Err(anyhow::Error::from(e)),
        };
        let entities = match entities {
            Ok(entities) => entities,
            Err(e) => {
                log_warn!("Failed to get power entities from {name}: {e:#}");
                continue;
            }
        };
        let names = residency_file_names(entities.into_iter().map(|entity| entity.name));
        if names.is_empty() {
            continue;
        }
        if device_added {
            log_warn!("Ignoring power residency entities from additional provider {name}");
            continue;
        }
        let Some(kernel) = kernel.upgrade() else {
            return Ok(());
        };
        let provider = match instance.connect_to_device_sync() {
            Ok(provider) => Arc::new(provider),
            Err(e) => {
                log_warn!("Failed to connect to power stats provider {name}: {e:#}");
                continue;
            }
        };
        let registry = &kernel.device_registry;
        registry.add_bus_device(
            RESIDENCY_DEVICE_NAME.into(),
            Some(registry.objects.platform_device()),
            registry.objects.platform_bus(),
            |device, dir| {
                build_device_directory(device, dir);
                for name in names {
                    let path = name.clone();
                    dir.entry(
                        &path,
                        BytesFile::new_node(PowerResidencyFile {
                            name,
                            provider: provider.clone(),
                        }),
                        mode!(IFREG, 0o444),
                    );
                }
            },
        );
        device_added = true;
    }
    Ok(())
}

/// Returns the names of the entities to serve as files: those with the prefix whose names are
/// safe to use as file names.
fn residency_file_names(names: impl IntoIterator<Item = String>) -> Vec<String> {
    names
        .into_iter()
        .filter(|name| {
            name.len() > RESIDENCY_FILE_PREFIX.len()
                && name.starts_with(RESIDENCY_FILE_PREFIX)
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
        .collect()
}

struct PowerResidencyFile {
    name: String,
    provider: Arc<fpowerstats::DeviceSynchronousProxy>,
}

impl BytesFileOps for PowerResidencyFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let reports = self.provider.get_stats(zx::MonotonicInstant::INFINITE).map_err(|e| {
            log_warn!("Failed to read power stats for {}: {e}", self.name);
            errno!(EIO)
        })?;
        let Some(stats) = reports.iter().find(|stats| stats.entity == self.name) else {
            log_warn!("Power stats provider no longer reports {}", self.name);
            return error!(EIO);
        };
        Ok(format_residency(stats).into_bytes().into())
    }
}

/// Formats the residency of a single power entity.
///
/// The power stats API does not report which state is current, so there is no `current_pf_state`
/// line.
fn format_residency(stats: &fpowerstats::Stats) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "num_pf_states: {}", stats.state_residency_data.len());
    for state in &stats.state_residency_data {
        let time_in_state_us = state.total_time_ms.saturating_mul(1000);
        let _ = writeln!(out, "pf_state: {}", state.id);
        let _ = writeln!(out, "pf_entry_count: {}", state.entry_count);
        let _ = writeln!(out, "pf_time_in_state: {time_in_state_us} (us)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::spawn_kernel_and_run;
    use fidl::endpoints::{ServerEnd, create_endpoints, create_sync_proxy};
    use fidl_fuchsia_io as fio;
    use fuchsia_async as fasync;
    use fuchsia_component::server::ServiceFs;
    use futures::{FutureExt, StreamExt, select};
    use std::pin::pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    /// Serves a fake provider that reports `reports` on `stream`, counting `GetEntities` calls.
    async fn serve_provider(
        stream: fpowerstats::DeviceRequestStream,
        reports: Vec<fpowerstats::Stats>,
        get_entities_calls: Arc<AtomicUsize>,
    ) {
        let _ = stream
            .for_each(|request| {
                let reports = &reports;
                let get_entities_calls = &get_entities_calls;
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
                            get_entities_calls.fetch_add(1, Ordering::SeqCst);
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

    /// Runs `serve` on its own thread, as residency files call providers synchronously.
    fn spawn_server<F: Future<Output = ()> + 'static>(serve: impl FnOnce() -> F + Send + 'static) {
        std::thread::spawn(move || fasync::LocalExecutor::default().run_singlethreaded(serve()));
    }

    /// Serves `fuchsia.hardware.power.stats.Service` with one instance per entry of `providers`.
    ///
    /// Returns the service and the number of `GetEntities` calls answered so far.
    fn serve_service(
        providers: Vec<(&'static str, Vec<fpowerstats::Stats>)>,
    ) -> (fclient::Service<fpowerstats::ServiceMarker>, Arc<AtomicUsize>) {
        let get_entities_calls = Arc::new(AtomicUsize::new(0));
        let (client, server) = create_endpoints::<fio::DirectoryMarker>();
        let calls = get_entities_calls.clone();
        spawn_server(move || async move {
            let mut fs = ServiceFs::new_local();
            for (instance, reports) in providers {
                let calls = calls.clone();
                fs.add_fidl_service_instance(instance, move |request| {
                    let fpowerstats::ServiceRequest::Device(stream) = request;
                    (stream, reports.clone(), calls.clone())
                });
            }
            fs.serve_connection(server).expect("serve service directory");
            fs.for_each_concurrent(None, |(stream, reports, calls)| {
                serve_provider(stream, reports, calls)
            })
            .await;
        });
        let service = fclient::Service::open_from_dir(client, fpowerstats::ServiceMarker)
            .expect("open service");
        (service, get_entities_calls)
    }

    /// Runs `watcher` until `done` returns true.
    async fn run_until<W>(mut watcher: W, done: impl Fn() -> bool)
    where
        W: Future<Output = Result<(), anyhow::Error>> + futures::future::FusedFuture + Unpin,
    {
        while !done() {
            select! {
                result = watcher => panic!("watcher exited: {result:?}"),
                _ = fasync::Timer::new(zx::MonotonicDuration::from_millis(10)).fuse() => {}
            }
        }
    }

    fn exists(kernel: &Kernel, path: &str) -> bool {
        kernel.device_registry.objects.root.lookup(path.into()).is_some()
    }

    #[::fuchsia::test]
    async fn device_is_added_with_prefixed_entities() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let p = RESIDENCY_FILE_PREFIX;
            let (service, _) = serve_service(vec![(
                "default",
                vec![
                    stats(&format!("{p}a"), vec![]),
                    stats(&format!("{p}b"), vec![]),
                    stats("other", vec![]),
                ],
            )]);
            let mut watcher = pin!(add_residency_device(kernel.weak_self.clone(), service).fuse());

            let device = format!("devices/platform/{RESIDENCY_DEVICE_NAME}");
            run_until(&mut watcher, || exists(kernel, &device)).await;

            assert!(exists(kernel, &format!("bus/platform/devices/{RESIDENCY_DEVICE_NAME}")));
            assert!(exists(kernel, &format!("{device}/uevent")));
            assert!(exists(kernel, &format!("{device}/{p}a")));
            assert!(exists(kernel, &format!("{device}/{p}b")));
            assert!(!exists(kernel, &format!("{device}/other")));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn no_device_without_prefixed_entities() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let (service, get_entities_calls) =
                serve_service(vec![("default", vec![stats("other", vec![])])]);
            let mut watcher = pin!(add_residency_device(kernel.weak_self.clone(), service).fuse());

            run_until(&mut watcher, || get_entities_calls.load(Ordering::SeqCst) == 1).await;

            assert!(!exists(kernel, &format!("devices/platform/{RESIDENCY_DEVICE_NAME}")));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn only_first_provider_is_served() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let p = RESIDENCY_FILE_PREFIX;
            let (service, get_entities_calls) = serve_service(vec![
                ("first", vec![stats(&format!("{p}a"), vec![])]),
                ("second", vec![stats(&format!("{p}b"), vec![])]),
            ]);
            let mut watcher = pin!(add_residency_device(kernel.weak_self.clone(), service).fuse());

            let device = format!("devices/platform/{RESIDENCY_DEVICE_NAME}");
            run_until(&mut watcher, || {
                get_entities_calls.load(Ordering::SeqCst) == 2 && exists(kernel, &device)
            })
            .await;

            let served_a = exists(kernel, &format!("{device}/{p}a"));
            let served_b = exists(kernel, &format!("{device}/{p}b"));
            assert!(served_a != served_b, "expected files from exactly one provider");
        })
        .await;
    }

    /// Returns a residency file for `name` backed by a fake provider reporting `reports`.
    fn residency_file(name: &str, reports: Vec<fpowerstats::Stats>) -> PowerResidencyFile {
        let (provider, server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
        spawn_server(move || serve_provider(server.into_stream(), reports, Default::default()));
        PowerResidencyFile { name: name.to_string(), provider: Arc::new(provider) }
    }

    #[::fuchsia::test]
    async fn read_returns_formatted_residency() {
        spawn_kernel_and_run(async |current_task| {
            let entity = stats("a", vec![state(0, 11, 2200)]);
            let file = residency_file("a", vec![stats("b", vec![]), entity.clone()]);

            let contents = file.read(current_task).expect("read");

            assert_eq!(&*contents, format_residency(&entity).as_bytes());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn read_fails_when_entity_is_no_longer_reported() {
        spawn_kernel_and_run(async |current_task| {
            let file = residency_file("a", vec![stats("b", vec![])]);

            assert_eq!(file.read(current_task).map(|_| ()), error!(EIO));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn read_fails_when_provider_is_gone() {
        spawn_kernel_and_run(async |current_task| {
            let (provider, server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            drop::<ServerEnd<_>>(server);
            let file = PowerResidencyFile { name: "a".to_string(), provider: Arc::new(provider) };

            assert_eq!(file.read(current_task).map(|_| ()), error!(EIO));
        })
        .await;
    }

    #[::fuchsia::test]
    fn residency_is_formatted_per_state() {
        let stats = fpowerstats::Stats {
            entity: "a".to_string(),
            state_residency_data: vec![state(0, 11, 2200), state(1, 4, 7)],
        };

        assert_eq!(
            format_residency(&stats),
            "num_pf_states: 2\n\
             pf_state: 0\npf_entry_count: 11\npf_time_in_state: 2200000 (us)\n\n\
             pf_state: 1\npf_entry_count: 4\npf_time_in_state: 7000 (us)\n\n"
        );
    }

    #[::fuchsia::test]
    fn residency_with_no_states() {
        let stats = fpowerstats::Stats { entity: "a".to_string(), state_residency_data: vec![] };

        assert_eq!(format_residency(&stats), "num_pf_states: 0\n");
    }

    #[::fuchsia::test]
    fn only_prefixed_entities_with_safe_names_are_served() {
        let p = RESIDENCY_FILE_PREFIX;
        let names = [
            format!("{p}a"),
            format!("{p}b_1"),
            "other".to_string(),
            p.to_string(),
            format!("{p}../x"),
            format!("{p}a/b"),
            p.to_uppercase() + "A",
            format!("x{p}a"),
        ];

        assert_eq!(residency_file_names(names), vec![format!("{p}a"), format!("{p}b_1")]);
    }
}
