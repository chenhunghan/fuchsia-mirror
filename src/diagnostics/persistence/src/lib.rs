// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `diagnostics-persistence` component persists Inspect VMOs and serves them at the next boot.

use anyhow::Error;
use argh::FromArgs;
use diagnostics_reader::{ArchiveReader, InspectArchiveReader};
use fidl::endpoints::{Proxy, Responder};
use fidl_fuchsia_diagnostics as fdiagnostics;
use fidl_fuchsia_diagnostics_persistence as fpersistence;
use fidl_fuchsia_hardware_power_statecontrol as fpower;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_power_battery as fbattery;
use fidl_fuchsia_update as fupdate;
use flate2::Compression as ZlibLevel;
use flate2::write::ZlibEncoder;
use fuchsia_async as fasync;
use fuchsia_component::client::{connect_to_protocol, connect_to_protocol_at_path};
use fuchsia_component::server::ServiceFs;
use fuchsia_inspect::health::Reporter;
use futures::lock::Mutex;
use futures::{FutureExt, StreamExt, TryStreamExt};
use log::*;
use persistence_build_config::Config;
use serde::{Deserialize, Serialize};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zx::{BootInstant, MonotonicDuration, MonotonicInstant};

/// The name of the subcommand and the logs-tag, used by launcher
pub const PROGRAM_NAME: &str = "persistence";

/// Percentage above threshold required to re-arm the low battery snapshot trigger.
const BATTERY_TRIGGER_HYSTERESIS_PERCENT: f32 = 5.0;

/// The Inspect snapshot: JSON (`InspectFormat::Json`) compressed as a single RFC 1950 zlib
/// stream (`Compression::ZlibRfc1950`).
const ACTIVE_FILE_NAME: &str = "active.json.zlib";
const ACTIVE_TMP_FILE_NAME: &str = "active.json.zlib.tmp";
const METADATA_FILE_NAME: &str = "metadata.json";
const METADATA_TMP_FILE_NAME: &str = "metadata.json.tmp";

/// serde_json issues many small writes. Batching them in front of the encoder keeps
/// compression about as fast as compressing a single buffer.
const SNAPSHOT_SERIALIZE_BUFFER_BYTES: usize = 64 * 1024;

/// Command line args
#[derive(FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "persistence")]
pub struct CommandLine {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Metadata {
    pub monotonic_timestamp: i64,
    pub boot_timestamp: i64,
    pub utc_timestamp_ns: Option<i64>,
}

enum IncomingService {
    PreviousBootDataProvider(fpersistence::PreviousBootDataProviderRequestStream),
}

pub async fn main(_args: CommandLine) -> Result<(), Error> {
    info!("Persistence starting up");
    let scope = fasync::Scope::new();

    let _inspect_controller = inspect_runtime::publish(
        fuchsia_inspect::component::inspector(),
        inspect_runtime::PublishOptions::default().custom_scope(scope.clone()),
    );

    fuchsia_inspect::component::health().set_starting_up();
    let config = Config::take_from_startup_handle();

    let cache_dir = Path::new("/cache");
    if let Err(e) = rotate_active_to_previous_boot(cache_dir) {
        warn!(e:?; "Error rotating active data to previous boot");
    }

    let (ready_tx, ready_rx) = futures::channel::oneshot::channel::<()>();
    let ready = ready_rx.shared();

    let mut fs = ServiceFs::new();
    fs.dir("svc").add_fidl_service(IncomingService::PreviousBootDataProvider);
    fs.take_and_serve_directory_handle()?;

    let cache_dir_buf = cache_dir.to_path_buf();
    let ready_for_handler = ready.clone();
    scope.spawn(async move {
        fs.for_each_concurrent(None, |IncomingService::PreviousBootDataProvider(stream)| {
            let cache_dir = cache_dir_buf.clone();
            let ready = ready_for_handler.clone();
            async move {
                if let Err(e) = handle_previous_boot_data_provider(stream, &cache_dir, ready).await
                {
                    warn!(e:?; "Error handling PreviousBootDataProvider request stream");
                }
            }
        })
        .await;
    });

    if config.skip_update_check {
        info!("Skipping the update check, publishing previous boot data");
        let _ = ready_tx.send(());
    } else {
        scope.spawn(async move {
            if let Err(e) = wait_for_update().await {
                warn!(e:?; "Will not publish previous boot data");
            } else {
                let _ = ready_tx.send(());
            }
        });
    }

    let proxy = connect_to_protocol_at_path::<fdiagnostics::ArchiveAccessorMarker>(
        "/svc/fuchsia.diagnostics.ArchiveAccessor.previous_boot",
    )?;

    let mut reader = ArchiveReader::inspect();
    reader.with_archive(proxy);
    let collector = Arc::new(Mutex::new(SnapshotCollector::new(reader, cache_dir.to_path_buf())));

    let period = MonotonicDuration::from_seconds(config.persistence_period_seconds);
    let collector_periodic = collector.clone();
    scope.spawn(async move {
        if let Err(e) = collector_periodic.lock().await.collect_active_snapshot().await {
            error!(e:?; "Error collecting initial active inspect snapshot");
        }
        let mut interval = fasync::Interval::new(period);
        while let Some(()) = interval.next().await {
            if let Err(e) = collector_periodic.lock().await.collect_active_snapshot().await {
                error!(e:?; "Error collecting active inspect snapshot");
            }
        }
    });

    if let Ok(battery_manager) = connect_to_protocol::<fbattery::BatteryManagerMarker>() {
        let threshold = config.low_battery_threshold_percent as f32;
        let collector_battery = collector.clone();
        scope.spawn(async move {
            if let Err(e) =
                listen_for_low_battery(battery_manager, threshold, collector_battery).await
            {
                warn!(e:?; "BatteryManager watcher task terminated");
            }
        });
    }

    if config.enable_shutdown_snapshot {
        if let Ok(shutdown_watcher_register) =
            connect_to_protocol::<fpower::ShutdownWatcherRegisterMarker>()
        {
            let collector_shutdown = collector.clone();
            scope.spawn(async move {
                if let Err(e) =
                    listen_for_shutdown(shutdown_watcher_register, collector_shutdown).await
                {
                    warn!(e:?; "Shutdown watcher task terminated");
                }
            });
        }
    } else {
        info!("Shutdown snapshot trigger disabled by configuration");
    }

    fuchsia_inspect::component::health().set_ok();
    scope.await;

    Ok(())
}

async fn listen_for_low_battery(
    battery_manager: fbattery::BatteryManagerProxy,
    threshold_percent: f32,
    collector: Arc<Mutex<SnapshotCollector>>,
) -> Result<(), Error> {
    let (watcher_client, mut request_stream) =
        fidl::endpoints::create_request_stream::<fbattery::BatteryInfoWatcherMarker>();
    battery_manager.watch(watcher_client)?;

    let mut triggered = false;

    while let Some(request) = request_stream.try_next().await? {
        match request {
            fbattery::BatteryInfoWatcherRequest::OnChangeBatteryInfo {
                info,
                wake_lease: _,
                responder,
            } => {
                let level_percent = info.level_percent.unwrap_or(100.0);
                let level_status = info.level_status.unwrap_or(fbattery::LevelStatus::Unknown);

                let is_low = level_percent <= threshold_percent
                    || level_status == fbattery::LevelStatus::Low
                    || level_status == fbattery::LevelStatus::Critical;

                if is_low {
                    if !triggered {
                        info!(
                            level_percent,
                            level_status:?;
                            "Low battery threshold reached, collecting active snapshot"
                        );
                        if let Err(e) = collector.lock().await.collect_active_snapshot().await {
                            error!(e:?; "Error collecting active snapshot on low battery");
                        }
                        triggered = true;
                    }
                } else if level_percent > threshold_percent + BATTERY_TRIGGER_HYSTERESIS_PERCENT {
                    triggered = false;
                }

                responder.send()?;
            }
        }
    }
    Ok(())
}

async fn listen_for_shutdown(
    shutdown_watcher_register: fpower::ShutdownWatcherRegisterProxy,
    collector: Arc<Mutex<SnapshotCollector>>,
) -> Result<(), Error> {
    let (watcher_client, mut request_stream) =
        fidl::endpoints::create_request_stream::<fpower::ShutdownWatcherMarker>();
    shutdown_watcher_register.register_watcher(watcher_client).await?;

    while let Some(request) = request_stream.try_next().await? {
        match request {
            fpower::ShutdownWatcherRequest::OnShutdown { options: _, responder } => {
                info!("Shutdown initiated, collecting active snapshot");
                if let Err(e) = collector.lock().await.collect_active_snapshot().await {
                    error!(e:?; "Error collecting active snapshot on shutdown");
                }
                let _ = responder.send();
                break;
            }
            fpower::ShutdownWatcherRequest::_UnknownMethod { .. } => {}
        }
    }
    Ok(())
}

fn rotate_active_to_previous_boot(cache_dir: &Path) -> Result<(), Error> {
    let active_dir = cache_dir.join("active");
    let previous_boot_dir = cache_dir.join("previous_boot");

    std::fs::create_dir_all(&active_dir)?;
    std::fs::create_dir_all(&previous_boot_dir)?;

    let active_file = active_dir.join(ACTIVE_FILE_NAME);
    let active_meta = active_dir.join(METADATA_FILE_NAME);
    let active_exists = active_file.exists();
    let meta_exists = active_meta.exists();

    info!(active_exists, meta_exists; "Rotating active data to previous boot");

    // Clean previous_boot directory first
    for entry in std::fs::read_dir(&previous_boot_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            let _ = std::fs::remove_file(path);
        }
    }

    // Only rotate a complete snapshot. Anything else left in the active directory, such as an
    // uncompressed `active.json` written by an older version, is removed below.
    if active_exists && meta_exists {
        std::fs::rename(&active_file, previous_boot_dir.join(ACTIVE_FILE_NAME))?;
        std::fs::rename(&active_meta, previous_boot_dir.join(METADATA_FILE_NAME))?;
    }

    // Ensure active directory is clean
    for entry in std::fs::read_dir(&active_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            let _ = std::fs::remove_file(path);
        }
    }

    Ok(())
}

fn maybe_get_utc_timestamp_from_clock<R: zx::Timeline, O: zx::Timeline>(
    clock: Option<&zx::Clock<R, O>>,
) -> Option<i64> {
    let clock = clock?;
    clock.read().ok().map(|time| time.into_nanos())
}

pub fn maybe_get_utc_timestamp() -> Option<i64> {
    let clock = fuchsia_runtime::duplicate_utc_clock_handle(zx::Rights::SAME_RIGHTS).ok();
    maybe_get_utc_timestamp_from_clock(clock.as_ref())
}

struct SnapshotCollector {
    reader: InspectArchiveReader,
    cache_dir: PathBuf,
}

impl SnapshotCollector {
    fn new(reader: InspectArchiveReader, cache_dir: PathBuf) -> Self {
        Self { reader, cache_dir }
    }

    async fn collect_active_snapshot(&mut self) -> Result<(), Error> {
        info!("Collecting active snapshot...");
        let inspect_data = match self.reader.snapshot().await {
            Ok(v) => v,
            Err(e) => {
                error!(e:?; "ArchiveReader snapshot failed");
                return Err(e.into());
            }
        };
        let active_dir = self.cache_dir.join("active");
        std::fs::create_dir_all(&active_dir)?;

        let metadata = Metadata {
            monotonic_timestamp: MonotonicInstant::get().into_nanos(),
            boot_timestamp: BootInstant::get().into_nanos(),
            utc_timestamp_ns: maybe_get_utc_timestamp(),
        };

        write_active_snapshot(&active_dir, &inspect_data, &metadata)
    }
}

/// Writes `inspect_data` (as zlib-compressed JSON) and `metadata` (as JSON) into `active_dir`.
/// Each file is written to a temporary file which is then renamed over the final file; a
/// temporary file is removed if either step fails.
fn write_active_snapshot<T: Serialize>(
    active_dir: &Path,
    inspect_data: &T,
    metadata: &Metadata,
) -> Result<(), Error> {
    let tmp_file = active_dir.join(ACTIVE_TMP_FILE_NAME);
    write_zlib_json(&tmp_file, inspect_data)
        .and_then(|()| {
            std::fs::rename(&tmp_file, active_dir.join(ACTIVE_FILE_NAME)).map_err(Error::from)
        })
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp_file);
        })?;

    let tmp_meta = active_dir.join(METADATA_TMP_FILE_NAME);
    std::fs::write(&tmp_meta, serde_json::to_vec(metadata)?)
        .and_then(|()| std::fs::rename(&tmp_meta, active_dir.join(METADATA_FILE_NAME)))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp_meta);
        })?;
    Ok(())
}

/// Streams `data` as JSON through a zlib (RFC 1950) encoder into a new file at `path`, without
/// holding the whole serialized snapshot in memory. On error, `path` is removed.
fn write_zlib_json<T: Serialize>(path: &Path, data: &T) -> Result<(), Error> {
    let file = std::fs::File::create(path)?;
    encode_zlib_json(BufWriter::new(file), data).inspect_err(|_| {
        let _ = std::fs::remove_file(path);
    })
}

/// Serializes `data` as JSON through a zlib (RFC 1950) encoder into `sink`.
fn encode_zlib_json<W: Write, T: Serialize>(sink: W, data: &T) -> Result<(), Error> {
    // flate2's default level (6): compressing once every few minutes makes the CPU cost
    // negligible, so take the smaller output.
    let encoder = ZlibEncoder::new(sink, ZlibLevel::default());
    let mut writer = BufWriter::with_capacity(SNAPSHOT_SERIALIZE_BUFFER_BYTES, encoder);
    serde_json::to_writer(&mut writer, data)?;
    let encoder = writer.into_inner().map_err(|e| e.into_error())?;
    // `finish` writes the end of the stream and the Adler-32 trailer.
    encoder.finish()?.flush()?;
    Ok(())
}

async fn handle_previous_boot_data_provider(
    mut stream: fpersistence::PreviousBootDataProviderRequestStream,
    cache_dir: &Path,
    ready: futures::future::Shared<futures::channel::oneshot::Receiver<()>>,
) -> Result<(), Error> {
    while let Some(request) = stream.try_next().await? {
        match request {
            fpersistence::PreviousBootDataProviderRequest::WatchPreviousBootData {
                options,
                responder,
            } => {
                // Both options are required: the stored snapshot is always zlib-compressed JSON,
                // and a client that leaves an option unset has not said it can consume it.
                if (options.format, options.compression)
                    != (
                        Some(fpersistence::InspectFormat::Json),
                        Some(fpersistence::Compression::ZlibRfc1950),
                    )
                {
                    warn!(options:?; "WatchPreviousBootData requires format JSON and compression ZLIB_RFC_1950; closing with NOT_SUPPORTED");
                    responder.control_handle().shutdown_with_epitaph(zx::Status::NOT_SUPPORTED);
                    return Ok(());
                }

                info!("Received WatchPreviousBootData request, awaiting ready signal...");
                if ready.clone().await.is_err() {
                    warn!("Update check failed or cancelled; will not serve previous boot data");
                    return Ok(());
                }
                info!("Ready signal received in WatchPreviousBootData handler");
                let previous_boot_dir = cache_dir.join("previous_boot");
                let active_file = previous_boot_dir.join(ACTIVE_FILE_NAME);
                let meta_file = previous_boot_dir.join(METADATA_FILE_NAME);

                if !active_file.exists() || !meta_file.exists() {
                    responder.send(fpersistence::PreviousBootData::default())?;
                    continue;
                }

                let meta_bytes = std::fs::read(&meta_file)?;
                let metadata: Metadata = serde_json::from_slice(&meta_bytes)?;

                let active_path_str =
                    active_file.to_str().ok_or_else(|| anyhow::anyhow!("Invalid path"))?;
                let file_proxy =
                    fuchsia_fs::file::open_in_namespace(active_path_str, fio::PERM_READABLE)?;
                let client_end = file_proxy
                    .into_client_end()
                    .map_err(|_| anyhow::anyhow!("Failed to convert to client end"))?;

                let data = fpersistence::PreviousBootData {
                    monotonic_timestamp: Some(MonotonicInstant::from_nanos(
                        metadata.monotonic_timestamp,
                    )),
                    boot_timestamp: Some(BootInstant::from_nanos(metadata.boot_timestamp)),
                    utc_timestamp_ns: metadata.utc_timestamp_ns,
                    inspect: Some(client_end),
                    format: Some(fpersistence::InspectFormat::Json),
                    compression: Some(fpersistence::Compression::ZlibRfc1950),
                    ..Default::default()
                };

                responder.send(data)?;
            }
            fpersistence::PreviousBootDataProviderRequest::_UnknownMethod { .. } => {}
        }
    }
    Ok(())
}

async fn wait_for_update() -> Result<(), Error> {
    info!("Waiting for post-boot update check...");
    let (notifier_client, mut notifier_request_stream) =
        fidl::endpoints::create_request_stream::<fupdate::NotifierMarker>();
    let proxy = connect_to_protocol::<fupdate::ListenerMarker>()?;
    proxy.notify_on_first_update_check(fupdate::ListenerNotifyOnFirstUpdateCheckRequest {
        notifier: Some(notifier_client),
        ..Default::default()
    })?;

    match notifier_request_stream.try_next().await {
        Ok(Some(fupdate::NotifierRequest::Notify { control_handle: _ })) => {}
        Ok(None) => {
            return Err(anyhow::anyhow!("Did not receive update notification; not publishing"));
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "Error waiting for update notification; not publishing: {e}"
            ));
        }
    }

    // Start serving previous boot data
    info!("...Update check has completed; publishing previous boot data");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::ZlibDecoder;
    use std::io::Read;
    use tempfile::TempDir;

    fn inflate(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        ZlibDecoder::new(bytes).read_to_end(&mut out).expect("valid zlib stream");
        out
    }

    fn write_previous_boot_files(cache_dir: &Path, meta: &Metadata) {
        let previous_boot_dir = cache_dir.join("previous_boot");
        std::fs::create_dir_all(&previous_boot_dir).unwrap();
        std::fs::write(
            previous_boot_dir.join(METADATA_FILE_NAME),
            serde_json::to_vec(meta).unwrap(),
        )
        .unwrap();
        std::fs::write(previous_boot_dir.join(ACTIVE_FILE_NAME), b"valid_inspect_zlib").unwrap();
    }

    fn serve_previous_boot_data(
        cache_dir: &Path,
    ) -> (fpersistence::PreviousBootDataProviderProxy, fasync::Scope) {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<
            fpersistence::PreviousBootDataProviderMarker,
        >();
        let (ready_tx, ready_rx) = futures::channel::oneshot::channel();
        let _ = ready_tx.send(());
        let ready = ready_rx.shared();
        let scope = fasync::Scope::new();
        let cache_dir = cache_dir.to_path_buf();
        scope.spawn(async move {
            let _ = handle_previous_boot_data_provider(stream, &cache_dir, ready).await;
        });
        (proxy, scope)
    }

    fn supported_options() -> fpersistence::PreviousBootDataProviderOptions {
        fpersistence::PreviousBootDataProviderOptions {
            format: Some(fpersistence::InspectFormat::Json),
            compression: Some(fpersistence::Compression::ZlibRfc1950),
            ..Default::default()
        }
    }

    #[test]
    fn test_metadata_serde() {
        let meta = Metadata {
            monotonic_timestamp: 1000,
            boot_timestamp: 2000,
            utc_timestamp_ns: Some(3000),
        };

        let serialized = serde_json::to_vec(&meta).unwrap();
        let deserialized: Metadata = serde_json::from_slice(&serialized).unwrap();
        assert_eq!(meta, deserialized);
    }

    #[test]
    fn test_metadata_serde_without_utc() {
        let meta =
            Metadata { monotonic_timestamp: 1000, boot_timestamp: 2000, utc_timestamp_ns: None };

        let serialized = serde_json::to_vec(&meta).unwrap();
        let deserialized: Metadata = serde_json::from_slice(&serialized).unwrap();
        assert_eq!(meta, deserialized);
    }

    #[test]
    fn test_maybe_get_utc_timestamp_when_utc_not_available() {
        // When no clock handle is available, returns None.
        assert_eq!(
            maybe_get_utc_timestamp_from_clock::<zx::MonotonicTimeline, zx::SyntheticTimeline>(
                None
            ),
            None
        );

        // When a clock handle without read rights is provided, read fails and returns None.
        let clock = zx::SyntheticClock::create(zx::ClockOpts::AUTO_START, None)
            .expect("failed to create clock");
        let no_read_clock = clock.duplicate_handle(zx::Rights::NONE).expect("duplicate handle");
        assert_eq!(maybe_get_utc_timestamp_from_clock(Some(&no_read_clock)), None);

        // When a valid readable clock is provided, returns Some timestamp.
        assert!(maybe_get_utc_timestamp_from_clock(Some(&clock)).is_some());
    }

    #[test]
    fn test_rotation() {
        let _ = std::fs::create_dir_all("/cache");
        let temp_dir = TempDir::new_in("/cache").unwrap();
        let cache_dir = temp_dir.path();
        let active_dir = cache_dir.join("active");
        let previous_boot_dir = cache_dir.join("previous_boot");

        std::fs::create_dir_all(&active_dir).unwrap();
        std::fs::write(active_dir.join(ACTIVE_FILE_NAME), b"active_data").unwrap();
        std::fs::write(active_dir.join(METADATA_FILE_NAME), b"meta_data").unwrap();

        rotate_active_to_previous_boot(cache_dir).unwrap();

        assert!(!active_dir.join(ACTIVE_FILE_NAME).exists());
        assert!(!active_dir.join(METADATA_FILE_NAME).exists());
        assert_eq!(
            std::fs::read(previous_boot_dir.join(ACTIVE_FILE_NAME)).unwrap(),
            b"active_data"
        );
        assert_eq!(
            std::fs::read(previous_boot_dir.join(METADATA_FILE_NAME)).unwrap(),
            b"meta_data"
        );
    }

    #[test]
    fn test_rotation_cleans_non_clean_active_dir() {
        let _ = std::fs::create_dir_all("/cache");
        let temp_dir = TempDir::new_in("/cache").unwrap();
        let cache_dir = temp_dir.path();
        let active_dir = cache_dir.join("active");
        let previous_boot_dir = cache_dir.join("previous_boot");

        std::fs::create_dir_all(&active_dir).unwrap();
        std::fs::write(active_dir.join(ACTIVE_FILE_NAME), b"active_data").unwrap();
        std::fs::write(active_dir.join(METADATA_FILE_NAME), b"meta_data").unwrap();
        // Add leftover temporary or stray files in active dir
        std::fs::write(active_dir.join(ACTIVE_TMP_FILE_NAME), b"tmp_active_data").unwrap();
        std::fs::write(active_dir.join(METADATA_TMP_FILE_NAME), b"tmp_meta_data").unwrap();
        std::fs::write(active_dir.join("stray.txt"), b"stray_data").unwrap();

        rotate_active_to_previous_boot(cache_dir).unwrap();

        // Active files should be rotated to previous_boot
        assert_eq!(
            std::fs::read(previous_boot_dir.join(ACTIVE_FILE_NAME)).unwrap(),
            b"active_data"
        );
        assert_eq!(
            std::fs::read(previous_boot_dir.join(METADATA_FILE_NAME)).unwrap(),
            b"meta_data"
        );
        // Stray files should not be in previous_boot
        assert!(!previous_boot_dir.join(ACTIVE_TMP_FILE_NAME).exists());
        assert!(!previous_boot_dir.join(METADATA_TMP_FILE_NAME).exists());
        assert!(!previous_boot_dir.join("stray.txt").exists());

        // Ensure active directory was completely cleaned
        assert_eq!(std::fs::read_dir(&active_dir).unwrap().count(), 0);
    }

    #[test]
    fn test_rotation_clears_stale_previous_boot_when_active_empty() {
        let _ = std::fs::create_dir_all("/cache");
        let temp_dir = TempDir::new_in("/cache").unwrap();
        let cache_dir = temp_dir.path();
        let active_dir = cache_dir.join("active");
        let previous_boot_dir = cache_dir.join("previous_boot");

        std::fs::create_dir_all(&active_dir).unwrap();
        std::fs::create_dir_all(&previous_boot_dir).unwrap();

        // Populate previous_boot with stale data from Boot N-2
        std::fs::write(previous_boot_dir.join(ACTIVE_FILE_NAME), b"stale_active_data").unwrap();
        std::fs::write(previous_boot_dir.join(METADATA_FILE_NAME), b"stale_meta_data").unwrap();

        // Active directory is empty (Boot N-1 captured no data)
        rotate_active_to_previous_boot(cache_dir).unwrap();

        // previous_boot directory MUST be cleared, not retaining stale Boot N-2 data
        assert!(!previous_boot_dir.join(ACTIVE_FILE_NAME).exists());
        assert!(!previous_boot_dir.join(METADATA_FILE_NAME).exists());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_handle_previous_boot_data_provider_missing_files() {
        let _ = std::fs::create_dir_all("/cache");
        let temp_dir = TempDir::new_in("/cache").unwrap();
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<
            fpersistence::PreviousBootDataProviderMarker,
        >();

        let (ready_tx, ready_rx) = futures::channel::oneshot::channel();
        let _ = ready_tx.send(());
        let ready = ready_rx.shared();

        let scope = fasync::Scope::new();
        let cache_dir = temp_dir.path().to_path_buf();
        scope.spawn(async move {
            let _ = handle_previous_boot_data_provider(stream, &cache_dir, ready).await;
        });

        let data = proxy.watch_previous_boot_data(&supported_options()).await.unwrap();
        assert!(data.inspect.is_none());
        assert!(data.monotonic_timestamp.is_none());
        assert!(data.boot_timestamp.is_none());
        assert!(data.utc_timestamp_ns.is_none());
        assert_eq!(data, fpersistence::PreviousBootData::default());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_handle_previous_boot_data_provider_valid_data() {
        let _ = std::fs::create_dir_all("/cache");
        let temp_dir = TempDir::new_in("/cache").unwrap();
        let previous_boot_dir = temp_dir.path().join("previous_boot");
        std::fs::create_dir_all(&previous_boot_dir).unwrap();

        let meta = Metadata {
            monotonic_timestamp: 12345,
            boot_timestamp: 67890,
            utc_timestamp_ns: Some(100000),
        };
        let meta_bytes = serde_json::to_vec(&meta).unwrap();
        std::fs::write(previous_boot_dir.join(METADATA_FILE_NAME), meta_bytes).unwrap();
        std::fs::write(previous_boot_dir.join(ACTIVE_FILE_NAME), b"valid_inspect_json").unwrap();

        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<
            fpersistence::PreviousBootDataProviderMarker,
        >();

        let (ready_tx, ready_rx) = futures::channel::oneshot::channel();
        let _ = ready_tx.send(());
        let ready = ready_rx.shared();

        let scope = fasync::Scope::new();
        let cache_dir = temp_dir.path().to_path_buf();
        scope.spawn(async move {
            let _ = handle_previous_boot_data_provider(stream, &cache_dir, ready).await;
        });

        let data = proxy.watch_previous_boot_data(&supported_options()).await.unwrap();
        assert_eq!(data.monotonic_timestamp, Some(MonotonicInstant::from_nanos(12345)));
        assert_eq!(data.boot_timestamp, Some(BootInstant::from_nanos(67890)));
        assert_eq!(data.utc_timestamp_ns, Some(100000));
        assert!(data.inspect.is_some());
        assert_eq!(data.format, Some(fpersistence::InspectFormat::Json));
        assert_eq!(data.compression, Some(fpersistence::Compression::ZlibRfc1950));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_handle_previous_boot_data_provider_utc_not_available() {
        let _ = std::fs::create_dir_all("/cache");
        let temp_dir = TempDir::new_in("/cache").unwrap();
        let previous_boot_dir = temp_dir.path().join("previous_boot");
        std::fs::create_dir_all(&previous_boot_dir).unwrap();

        let meta =
            Metadata { monotonic_timestamp: 12345, boot_timestamp: 67890, utc_timestamp_ns: None };
        let meta_bytes = serde_json::to_vec(&meta).unwrap();
        std::fs::write(previous_boot_dir.join(METADATA_FILE_NAME), meta_bytes).unwrap();
        std::fs::write(previous_boot_dir.join(ACTIVE_FILE_NAME), b"valid_inspect_json").unwrap();

        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<
            fpersistence::PreviousBootDataProviderMarker,
        >();

        let (ready_tx, ready_rx) = futures::channel::oneshot::channel();
        let _ = ready_tx.send(());
        let ready = ready_rx.shared();

        let scope = fasync::Scope::new();
        let cache_dir = temp_dir.path().to_path_buf();
        scope.spawn(async move {
            let _ = handle_previous_boot_data_provider(stream, &cache_dir, ready).await;
        });

        let data = proxy.watch_previous_boot_data(&supported_options()).await.unwrap();
        assert_eq!(data.monotonic_timestamp, Some(MonotonicInstant::from_nanos(12345)));
        assert_eq!(data.boot_timestamp, Some(BootInstant::from_nanos(67890)));
        assert_eq!(data.utc_timestamp_ns, None);
        assert!(data.inspect.is_some());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_wait_for_update_no_listener() {
        let res = wait_for_update().await;
        assert!(res.is_err());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_handle_previous_boot_data_provider_canceled_ready() {
        let _ = std::fs::create_dir_all("/cache");
        let temp_dir = TempDir::new_in("/cache").unwrap();
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<
            fpersistence::PreviousBootDataProviderMarker,
        >();

        let (ready_tx, ready_rx) = futures::channel::oneshot::channel::<()>();
        drop(ready_tx);
        let ready = ready_rx.shared();

        let scope = fasync::Scope::new();
        let cache_dir = temp_dir.path().to_path_buf();
        scope.spawn(async move {
            let _ = handle_previous_boot_data_provider(stream, &cache_dir, ready).await;
        });

        let res = proxy.watch_previous_boot_data(&supported_options()).await;
        assert!(res.is_err());
    }

    #[test]
    fn test_write_active_snapshot_round_trips_zlib() {
        let _ = std::fs::create_dir_all("/cache");
        let temp_dir = TempDir::new_in("/cache").unwrap();
        let active_dir = temp_dir.path();
        let inspect_data =
            serde_json::json!([{"moniker": "core/foo", "payload": {"a": [1, 2, 3]}}]);
        let meta = Metadata {
            monotonic_timestamp: 1000,
            boot_timestamp: 2000,
            utc_timestamp_ns: Some(3000),
        };

        write_active_snapshot(active_dir, &inspect_data, &meta).unwrap();

        let bytes = std::fs::read(active_dir.join(ACTIVE_FILE_NAME)).unwrap();
        // RFC 1950 header: CM = 8 (DEFLATE), and CMF/FLG is a multiple of 31.
        assert_eq!(bytes[0] & 0x0f, 8);
        assert_eq!(u16::from_be_bytes([bytes[0], bytes[1]]) % 31, 0);
        assert_eq!(inflate(&bytes), serde_json::to_vec(&inspect_data).unwrap());
        let meta_bytes = std::fs::read(active_dir.join(METADATA_FILE_NAME)).unwrap();
        assert_eq!(serde_json::from_slice::<Metadata>(&meta_bytes).unwrap(), meta);
        assert!(!active_dir.join(ACTIVE_TMP_FILE_NAME).exists());
        assert!(!active_dir.join(METADATA_TMP_FILE_NAME).exists());
    }

    #[test]
    fn test_write_zlib_json_removes_file_on_error() {
        let _ = std::fs::create_dir_all("/cache");
        let temp_dir = TempDir::new_in("/cache").unwrap();
        let path = temp_dir.path().join(ACTIVE_TMP_FILE_NAME);
        // serde_json cannot serialize a map with non-string keys.
        let unserializable = std::collections::BTreeMap::from([((1, 2), 3)]);

        assert!(write_zlib_json(&path, &unserializable).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn test_rotation_discards_legacy_uncompressed_active_json() {
        let _ = std::fs::create_dir_all("/cache");
        let temp_dir = TempDir::new_in("/cache").unwrap();
        let cache_dir = temp_dir.path();
        let active_dir = cache_dir.join("active");
        let previous_boot_dir = cache_dir.join("previous_boot");

        // Files left behind by a version that stored uncompressed JSON.
        std::fs::create_dir_all(&active_dir).unwrap();
        std::fs::write(active_dir.join("active.json"), b"[]").unwrap();
        std::fs::write(active_dir.join(METADATA_FILE_NAME), b"meta_data").unwrap();

        rotate_active_to_previous_boot(cache_dir).unwrap();

        assert_eq!(std::fs::read_dir(&active_dir).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&previous_boot_dir).unwrap().count(), 0);
    }

    #[test]
    fn test_rotation_requires_complete_snapshot() {
        let _ = std::fs::create_dir_all("/cache");
        let temp_dir = TempDir::new_in("/cache").unwrap();
        let cache_dir = temp_dir.path();
        let active_dir = cache_dir.join("active");
        let previous_boot_dir = cache_dir.join("previous_boot");
        std::fs::create_dir_all(&active_dir).unwrap();

        std::fs::write(active_dir.join(ACTIVE_FILE_NAME), b"active_data").unwrap();
        rotate_active_to_previous_boot(cache_dir).unwrap();
        assert_eq!(std::fs::read_dir(&active_dir).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&previous_boot_dir).unwrap().count(), 0);

        std::fs::write(active_dir.join(METADATA_FILE_NAME), b"meta_data").unwrap();
        rotate_active_to_previous_boot(cache_dir).unwrap();
        assert_eq!(std::fs::read_dir(&active_dir).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&previous_boot_dir).unwrap().count(), 0);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_snapshot_is_served_compressed_after_rotation() {
        let _ = std::fs::create_dir_all("/cache");
        let temp_dir = TempDir::new_in("/cache").unwrap();
        let cache_dir = temp_dir.path();
        let active_dir = cache_dir.join("active");
        std::fs::create_dir_all(&active_dir).unwrap();
        let inspect_data = serde_json::json!([{"moniker": "core/foo", "payload": {"b": "c"}}]);
        let meta =
            Metadata { monotonic_timestamp: 1, boot_timestamp: 2, utc_timestamp_ns: Some(3) };
        write_active_snapshot(&active_dir, &inspect_data, &meta).unwrap();
        rotate_active_to_previous_boot(cache_dir).unwrap();

        let (proxy, _scope) = serve_previous_boot_data(cache_dir);
        let data = proxy.watch_previous_boot_data(&supported_options()).await.unwrap();

        assert_eq!(data.monotonic_timestamp, Some(MonotonicInstant::from_nanos(1)));
        assert_eq!(data.format, Some(fpersistence::InspectFormat::Json));
        assert_eq!(data.compression, Some(fpersistence::Compression::ZlibRfc1950));
        let file = data.inspect.unwrap().into_proxy();
        let bytes = fuchsia_fs::file::read(&file).await.unwrap();
        assert_eq!(inflate(&bytes), serde_json::to_vec(&inspect_data).unwrap());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_handle_previous_boot_data_provider_rejects_unsupported_options() {
        let _ = std::fs::create_dir_all("/cache");
        let temp_dir = TempDir::new_in("/cache").unwrap();
        let meta =
            Metadata { monotonic_timestamp: 12345, boot_timestamp: 67890, utc_timestamp_ns: None };
        write_previous_boot_files(temp_dir.path(), &meta);

        let json = Some(fpersistence::InspectFormat::Json);
        let zlib = Some(fpersistence::Compression::ZlibRfc1950);
        let unknown_format =
            Some(fpersistence::InspectFormat::from_primitive_allow_unknown(0xdead));
        let unknown_compression =
            Some(fpersistence::Compression::from_primitive_allow_unknown(0xdead));
        for (format, compression) in [
            (None, None),
            (json, None),
            (None, zlib),
            (unknown_format, zlib),
            (json, unknown_compression),
        ] {
            let options = fpersistence::PreviousBootDataProviderOptions {
                format,
                compression,
                ..Default::default()
            };
            let (proxy, _scope) = serve_previous_boot_data(temp_dir.path());
            let res = proxy.watch_previous_boot_data(&options).await;
            assert!(
                matches!(
                    res,
                    Err(fidl::Error::ClientChannelClosed {
                        epitaph: fidl::Epitaph::Explicit(Err(zx::Status::NOT_SUPPORTED)),
                        ..
                    })
                ),
                "unexpected result for {options:?}: {res:?}"
            );
        }
    }
}
