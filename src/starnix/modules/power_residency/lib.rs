// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Exposes power residency entities from [`PowerStatsProvider`] instances under platform devices in
//! `/sys/devices/platform`, such as `powerdashboard` and `cap_sysfs`.
//!
//! These devices are not generic Linux sysfs interfaces. They emulate the SoC-specific vendor
//! devices that the Android PowerStats HAL reads residency from: the CPM power dashboard and the
//! CAP (CPU Auxiliary Processor) stats device. The DSU's devfreq device is also emulated, as the
//! HAL reads the DSU's frequencies from it to label the DSU's DVFS residency.

#![recursion_limit = "256"]

use fidl_fuchsia_hardware_power_stats as fpowerstats;
use starnix_core::device::power_stats::PowerStatsProvider;
use starnix_core::device::service_watcher::ServiceBindingScope;
use starnix_core::fs::sysfs::build_device_directory;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::pseudo::simple_file::{BytesFile, BytesFileOps};
use starnix_logging::log_warn;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

/// Power entities whose names start with this prefix are served as performance state residency
/// files of the same name.
const PERF_STATE_FILE_PREFIX: &str = "lpcm_";

/// Power entities whose names start with this prefix are served as power domain residency files
/// of the same name.
const POWER_DOMAIN_FILE_PREFIX: &str = "pwrblk_";

/// Power entities whose names start with this prefix, followed by the name of a platform power
/// state, are served as power state blocker files of the same name.
const POWER_STATE_BLOCKERS_FILE_PREFIX: &str = "blockers_";

/// Power entity served as the platform power state residency file.
const PLATFORM_POWER_ENTITY: &str = "soc_power_state";

/// Name of the file [`PLATFORM_POWER_ENTITY`] is served as.
const PLATFORM_POWER_FILE: &str = "platform_power_residency";

/// Name of the platform device under which the residency files are served, emulating the
/// SoC-specific CPM power dashboard device read by the PowerStats HAL.
const RESIDENCY_DEVICE_NAME: &str = "powerdashboard";

/// Power entity served as the power state stats file.
const POWER_STATE_STATS_ENTITY: &str = "cap_power_state";

/// Name of the file [`POWER_STATE_STATS_ENTITY`] is served as.
const POWER_STATE_STATS_FILE: &str = "power_state_stats";

/// Name of the platform device under which [`POWER_STATE_STATS_FILE`] and the DVFS stats files are
/// served, emulating the SoC-specific CAP stats device read by the PowerStats HAL.
const CAP_SYSFS_DEVICE_NAME: &str = "cap_sysfs";

/// Power entities whose names start with this prefix, followed by the name of a clock domain, are
/// served as DVFS stats files. Like [`POWER_STATE_STATS_ENTITY`], the prefix names CAP, so that
/// DVFS entities of other components are not served under [`CAP_SYSFS_DEVICE_NAME`].
const DVFS_STATS_ENTITY_PREFIX: &str = "cap_dvfs_";

/// Prefix of the name of the DVFS stats file of each clock domain, which is followed by the name of
/// the clock domain.
const DVFS_STATS_FILE_PREFIX: &str = "dvfs_stats_";

/// Power entity reporting the DSU's DVFS residency. When a provider reports it, the DSU's devfreq
/// device is also served, as the frequency list for that residency.
const DSU_DVFS_ENTITY: &str = "cap_dvfs_clk_dsu";

/// Name of the DSU's platform device, and of its devfreq device.
const DSU_DEVFREQ_DEVICE_NAME: &str = "200c0780.dsufreq";

/// Frequencies of the DSU's operating points in Hz, from slowest to fastest.
///
/// The DSU has no frequency driver, so the frequencies are those its devfreq device lists on
/// Linux. The fastest one is CAP operating level 3 and the slowest is level 23.
// TODO(b/578170862): Get the frequencies from the power stats provider.
const DSU_AVAILABLE_FREQUENCIES_HZ: [u64; 21] = [
    115_000_000,
    268_000_000,
    400_000_000,
    537_000_000,
    691_000_000,
    768_000_000,
    844_000_000,
    960_000_000,
    1_056_000_000,
    1_132_000_000,
    1_228_000_000,
    1_324_000_000,
    1_420_000_000,
    1_516_000_000,
    1_612_000_000,
    1_728_000_000,
    1_843_000_000,
    1_920_000_000,
    1_996_000_000,
    2_092_000_000,
    2_188_000_000,
];

/// Ticks per millisecond of the counter that some residency files report timestamps in.
///
/// Those files report raw values of the platform timer counter, which is expected to run at
/// 38.4 MHz, so the provider's millisecond timestamps are converted at 38,400 ticks per
/// millisecond.
const TIMESTAMP_TICKS_PER_MS: u64 = 38_400;

/// Formats the residency of a power entity as the contents of its residency file.
type FormatResidency = fn(&fpowerstats::Stats) -> String;

/// Where and how a power entity is served.
struct ResidencyFileSpec {
    /// Name of the platform device under which the file is served.
    device: &'static str,
    /// Name of the file within the device directory.
    file: String,
    /// Name of the power entity whose residency the file serves.
    entity: String,
    format: FormatResidency,
}

/// Serves a file for each power entity reported over `fuchsia.hardware.power.stats.Service` that
/// [`residency_file_spec`] maps to one, under its platform device.
///
/// The entities are only known once a provider has started, so each device is added asynchronously
/// via [`PowerStatsProvider::watch`], once a provider reports at least one entity served under it.
/// All attributes for a platform device are populated atomically from that single provider;
/// entities for the same device from any concurrent providers are ignored. If the provider
/// disconnects, its platform devices are removed so a restarted provider can re-create them.
///
/// The DSU's devfreq device is added along with [`DSU_DVFS_ENTITY`]'s file.
pub fn init_power_residency_device(kernel: &Kernel) {
    PowerStatsProvider::watch(kernel, |current_task, provider, scope| {
        add_power_residency_devices(current_task.kernel(), provider, scope);
    });
}

/// Adds platform devices and sysfs residency files for the power entities reported by `provider`,
/// bound to `scope`.
fn add_power_residency_devices(
    kernel: &Kernel,
    provider: &Arc<PowerStatsProvider>,
    scope: &mut ServiceBindingScope,
) {
    let mut files_by_device: HashMap<&'static str, Vec<ResidencyFileSpec>> = HashMap::new();
    for spec in provider.entity_names().iter().filter_map(|name| residency_file_spec(name)) {
        files_by_device.entry(spec.device).or_default().push(spec);
    }
    let registry = &kernel.device_registry;
    for (device_name, files) in files_by_device {
        if registry
            .objects
            .root
            .lookup(format!("devices/platform/{device_name}").as_str().into())
            .is_some()
        {
            log_warn!(
                "Ignoring {device_name} power residency entities from additional provider {}",
                provider.instance_name()
            );
            continue;
        }
        let has_dsu = files.iter().any(|spec| spec.entity == DSU_DVFS_ENTITY);
        let provider_for_files = provider.clone();
        let device = registry.add_bus_device(
            device_name.into(),
            Some(registry.objects.platform_device()),
            registry.objects.platform_bus(),
            |device, dir| {
                build_device_directory(device, dir);
                for spec in files {
                    let file = ResidencyFile {
                        entity: spec.entity,
                        format: spec.format,
                        provider: provider_for_files.clone(),
                    };
                    dir.entry(&spec.file, BytesFile::new_node(file), mode!(IFREG, 0o444));
                }
            },
        );
        scope.add_scoped_device(device);
        if has_dsu {
            add_dsu_devfreq_device(kernel, scope);
        }
    }
}

/// Adds the DSU's platform device and its devfreq device, which lists the DSU's frequencies in
/// `available_frequencies`.
fn add_dsu_devfreq_device(kernel: &Kernel, scope: &mut ServiceBindingScope) {
    let registry = &kernel.device_registry;
    let platform_device = scope.add_scoped_device(registry.add_bus_device(
        DSU_DEVFREQ_DEVICE_NAME.into(),
        Some(registry.objects.platform_device()),
        registry.objects.platform_bus(),
        |device, dir| build_device_directory(device, dir),
    ));
    let devfreq_class = registry.objects.get_or_create_class("devfreq".into());
    scope.add_scoped_device(registry.add_numberless_device(
        DSU_DEVFREQ_DEVICE_NAME.into(),
        Some(platform_device),
        devfreq_class,
        |device, dir| {
            build_device_directory(device, dir);
            dir.entry(
                "available_frequencies",
                BytesFile::new_node(dsu_available_frequencies().into_bytes()),
                mode!(IFREG, 0o444),
            );
        },
    ));
}

/// Returns the contents of the DSU's `available_frequencies` file: its frequencies in Hz, from
/// slowest to fastest, separated by spaces.
fn dsu_available_frequencies() -> String {
    let frequencies: Vec<_> = DSU_AVAILABLE_FREQUENCIES_HZ.iter().map(u64::to_string).collect();
    frequencies.join(" ") + "\n"
}

/// Returns where and how to serve the power entity named `entity`, or [`None`] if it is not
/// served. Prefixed entities are only served if their names are safe to use as file names.
fn residency_file_spec(entity: &str) -> Option<ResidencyFileSpec> {
    let prefixed = |prefix: &str| {
        entity.len() > prefix.len()
            && entity.starts_with(prefix)
            && entity.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    };
    let (device, file, format): (_, _, FormatResidency) = if prefixed(PERF_STATE_FILE_PREFIX) {
        (RESIDENCY_DEVICE_NAME, entity.to_string(), format_performance_states)
    } else if prefixed(POWER_DOMAIN_FILE_PREFIX) {
        (RESIDENCY_DEVICE_NAME, entity.to_string(), format_power_domain_states)
    } else if prefixed(POWER_STATE_BLOCKERS_FILE_PREFIX) {
        (RESIDENCY_DEVICE_NAME, entity.to_string(), format_power_state_blockers)
    } else if entity == PLATFORM_POWER_ENTITY {
        (RESIDENCY_DEVICE_NAME, PLATFORM_POWER_FILE.to_string(), format_platform_power_states)
    } else if prefixed(DVFS_STATS_ENTITY_PREFIX) {
        let file = format!("{DVFS_STATS_FILE_PREFIX}{}", &entity[DVFS_STATS_ENTITY_PREFIX.len()..]);
        (CAP_SYSFS_DEVICE_NAME, file, format_dvfs_stats)
    } else if entity == POWER_STATE_STATS_ENTITY {
        (CAP_SYSFS_DEVICE_NAME, POWER_STATE_STATS_FILE.to_string(), format_power_state_stats)
    } else {
        return None;
    };
    Some(ResidencyFileSpec { device, file, entity: entity.to_string(), format })
}

/// Serves the residency of a single power entity from `provider`, as formatted by `format`.
struct ResidencyFile {
    entity: String,
    format: FormatResidency,
    provider: Arc<PowerStatsProvider>,
}

impl BytesFileOps for ResidencyFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let stats = self.provider.entity_stats(&self.entity)?;
        Ok((self.format)(&stats).into_bytes().into())
    }
}

/// Returns the states in `stats` whose names are safe to write to a line-based residency file,
/// logging a warning for any state left out.
///
/// States whose names contain control characters, such as newlines, are left out, as their names
/// would break the line-based layout.
fn safe_states(stats: &fpowerstats::Stats) -> impl Iterator<Item = &fpowerstats::StateResidency> {
    stats.state_residency_data.iter().filter(|state| {
        if state.name.chars().any(char::is_control) {
            log_warn!("Leaving out {} state {} with an unsafe name", stats.entity, state.id);
            return false;
        }
        true
    })
}

/// Formats `stats` as a `num_pf_states` line followed by `pf_*` lines for each performance level.
///
/// The power stats API does not report times with sub-millisecond precision, so times are whole
/// milliseconds converted to microseconds.
fn format_performance_states(stats: &fpowerstats::Stats) -> String {
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

/// Formats `stats` as an `SSWRP lpb` header followed by `power_state` lines for each power
/// state.
///
/// States whose names contain control characters, such as newlines, are left out, as their names
/// would break the line-based layout.
///
/// The power stats API does not report times with sub-millisecond precision or when a state was
/// last exited, so times are whole milliseconds converted to microseconds and last exit
/// timestamps are 0.
fn format_power_domain_states(stats: &fpowerstats::Stats) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\nSSWRP lpb\n");
    for state in safe_states(stats) {
        let time_in_state_us = state.total_time_ms.saturating_mul(1000);
        let last_entry_us = state.last_entry_timestamp_ms.saturating_mul(1000);
        let _ = writeln!(out, "power_state: {}", state.name);
        let _ = writeln!(out, "entry_count: {}", state.entry_count);
        let _ = writeln!(out, "time_in_state: {time_in_state_us} (us)");
        let _ = writeln!(out, "last_entry_timestamp: {last_entry_us}");
        let _ = writeln!(out, "last_exit_timestamp: 0");
    }
    out.push('\n');
    out
}

/// Formats `stats` as a `power_state` line followed by counts and timestamps for each power
/// state.
///
/// States whose names contain control characters, such as newlines, are left out, as their names
/// would break the line-based layout.
///
/// The power stats API does not report which state is current or when a state was last exited,
/// so there are no current state lines and last exit timestamps are 0.
fn format_platform_power_states(stats: &fpowerstats::Stats) -> String {
    let mut out = String::new();
    for state in safe_states(stats) {
        let last_entry_ticks = state.last_entry_timestamp_ms.saturating_mul(TIMESTAMP_TICKS_PER_MS);
        let _ = writeln!(out, "power_state: {}", state.name);
        let _ = writeln!(out, "entry_count: {}", state.entry_count);
        let _ = writeln!(out, "time_in_state_ms: {}", state.total_time_ms);
        let _ = writeln!(out, "last_entry_timestamp_ticks: {last_entry_ticks}");
        let _ = writeln!(out, "last_exit_timestamp_ticks: 0\n");
    }
    out
}

/// Formats `stats` as a header naming the blocked platform power state (the entity's name after
/// [`POWER_STATE_BLOCKERS_FILE_PREFIX`], in upper case), followed by a `Blocker` record for each
/// state of the entity, which is a blocker of that power state.
///
/// Each blocker's entry count, residency, and last entry timestamp are the number of times it
/// blocked the power state, the time it blocked it for, and when it last started blocking it.
///
/// States whose names contain control characters, such as newlines, are left out, as their names
/// would break the line-based layout.
fn format_power_state_blockers(stats: &fpowerstats::Stats) -> String {
    let power_state =
        stats.entity.strip_prefix(POWER_STATE_BLOCKERS_FILE_PREFIX).unwrap_or(&stats.entity);
    let mut out = String::new();
    // The power stats API does not report which blockers are currently blocking, so unlike the
    // Linux files, there is no current blockers list and no `Currently blocking` lines.
    let _ = writeln!(out, "\n[{}]\n", power_state.to_ascii_uppercase());
    for state in safe_states(stats) {
        let last_blocked_ticks =
            state.last_entry_timestamp_ms.saturating_mul(TIMESTAMP_TICKS_PER_MS);
        let _ = writeln!(out, "Blocker: {}", state.name);
        let _ = writeln!(out, "Blocked Count: {}", state.entry_count);
        let _ = writeln!(out, "Last Blocked Timestamp (ticks): {last_blocked_ticks}");
        // The power stats API reports a residency for every state, so every record has a
        // `Blocked Time (ms)` line, unlike the Linux files, where the records of blockers tracked
        // by the kernel have none.
        let _ = writeln!(out, "Blocked Time (ms): {}\n", state.total_time_ms);
    }
    out
}

/// Formats `stats` as `---`-delimited records of `state`, timestamps, residency, and entry count
/// for each power state.
///
/// States whose names contain control characters, such as newlines, are left out, as their names
/// would break the line-based layout.
///
/// The power stats API does not report when a state was last exited, so last exit timestamps are
/// 0.
fn format_power_state_stats(stats: &fpowerstats::Stats) -> String {
    let mut out = String::new();
    for state in safe_states(stats) {
        let last_entry_ticks = state.last_entry_timestamp_ms.saturating_mul(TIMESTAMP_TICKS_PER_MS);
        let _ = writeln!(out, "---");
        let _ = writeln!(out, "state: {}", state.name);
        let _ = writeln!(out, "last_entry_ts_ticks: {last_entry_ticks}");
        let _ = writeln!(out, "last_exit_ts_ticks: 0");
        let _ = writeln!(out, "total_state_residency_ms: {}", state.total_time_ms);
        let _ = writeln!(out, "last_state_entry_counts: {}", state.entry_count);
    }
    out
}

/// Formats `stats` as a record that starts with `---` and names the clock domain, followed by the
/// entry count, residency, and last entry timestamp of each operating level.
///
/// Operating levels are identified by their state IDs. The power stats API does not report the
/// level a clock domain is at or is moving to, so there are no current or target level lines.
fn format_dvfs_stats(stats: &fpowerstats::Stats) -> String {
    let clock_domain = stats.entity.strip_prefix(DVFS_STATS_ENTITY_PREFIX).unwrap_or(&stats.entity);
    let mut out = String::new();
    let _ = writeln!(out, "---");
    let _ = writeln!(out, "clock_domain: {clock_domain}");
    let _ = writeln!(out, "per_op_level_stats: ");
    for state in &stats.state_residency_data {
        let last_entry_ticks = state.last_entry_timestamp_ms.saturating_mul(TIMESTAMP_TICKS_PER_MS);
        let _ = writeln!(out, "  - oplevel: {}", state.id);
        let _ = writeln!(out, "    entry_count: {}", state.entry_count);
        let _ = writeln!(out, "    total_residency_ms: {}", state.total_time_ms);
        let _ = writeln!(out, "    last_entry_timestamp_ticks: {last_entry_ticks}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::{ServerEnd, create_sync_proxy};
    use fuchsia_async as fasync;
    use futures::StreamExt;
    use starnix_core::testing::spawn_kernel_and_run_sync;
    use starnix_uapi::error;

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

    /// Serves a fake provider that reports `reports` on `stream`.
    async fn serve_provider(
        stream: fpowerstats::DeviceRequestStream,
        reports: Vec<fpowerstats::Stats>,
    ) {
        let _ = stream
            .for_each(|request| {
                let reports = &reports;
                async move {
                    if let Ok(fpowerstats::DeviceRequest::GetStats { responder }) = request {
                        let _ = responder.send(reports);
                    }
                }
            })
            .await;
    }

    /// Runs `serve` on its own thread, as residency files call providers synchronously.
    fn spawn_server<F: Future<Output = ()> + 'static>(serve: impl FnOnce() -> F + Send + 'static) {
        std::thread::spawn(move || fasync::LocalExecutor::default().run_singlethreaded(serve()));
    }

    /// Returns a [`PowerStatsProvider`] reporting `entity_names` for tests that only exercise
    /// device registration.
    fn provider_with_entities(
        instance_name: &str,
        entity_names: impl IntoIterator<Item = impl Into<String>>,
    ) -> PowerStatsProvider {
        fake_provider(
            instance_name,
            entity_names.into_iter().map(|name| stats(&name.into(), vec![])).collect(),
        )
    }

    /// Returns a fake [`PowerStatsProvider`] named `instance_name` that serves `reports`.
    fn fake_provider(instance_name: &str, reports: Vec<fpowerstats::Stats>) -> PowerStatsProvider {
        let entity_names: Vec<_> = reports.iter().map(|stats| stats.entity.clone()).collect();
        let (proxy, server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
        spawn_server(move || serve_provider(server.into_stream(), reports));
        PowerStatsProvider::new(instance_name, entity_names, proxy)
    }

    /// Returns a performance state residency file serving `entity` from `provider`.
    fn perf_file(entity: &str, provider: Arc<PowerStatsProvider>) -> ResidencyFile {
        ResidencyFile { entity: entity.to_string(), format: format_performance_states, provider }
    }

    /// Adds residency devices for `provider` in `kernel`.
    fn bind_provider(kernel: &Kernel, provider: PowerStatsProvider) {
        let mut scope = ServiceBindingScope::default();
        add_power_residency_devices(kernel, &Arc::new(provider), &mut scope);
    }

    fn exists(kernel: &Kernel, path: &str) -> bool {
        kernel.device_registry.objects.root.lookup(path.into()).is_some()
    }

    #[::fuchsia::test]
    async fn device_is_added_with_served_entities() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let prefix = PERF_STATE_FILE_PREFIX;
            bind_provider(
                kernel,
                provider_with_entities(
                    "default",
                    [
                        format!("{prefix}a"),
                        format!("{prefix}b"),
                        PLATFORM_POWER_ENTITY.to_string(),
                        "other".to_string(),
                    ],
                ),
            );

            let device = format!("devices/platform/{RESIDENCY_DEVICE_NAME}");
            assert!(exists(kernel, &device));
            assert!(exists(kernel, &format!("bus/platform/devices/{RESIDENCY_DEVICE_NAME}")));
            assert!(exists(kernel, &format!("{device}/uevent")));
            assert!(exists(kernel, &format!("{device}/{prefix}a")));
            assert!(exists(kernel, &format!("{device}/{prefix}b")));
            assert!(exists(kernel, &format!("{device}/{PLATFORM_POWER_FILE}")));
            assert!(!exists(kernel, &format!("{device}/other")));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn no_device_without_served_entities() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            bind_provider(kernel, provider_with_entities("default", ["other"]));

            assert!(!exists(kernel, &format!("devices/platform/{RESIDENCY_DEVICE_NAME}")));
            assert!(!exists(kernel, &format!("devices/platform/{CAP_SYSFS_DEVICE_NAME}")));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn device_is_added_when_later_provider_reports_served_entities() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let prefix = PERF_STATE_FILE_PREFIX;
            let device = format!("devices/platform/{RESIDENCY_DEVICE_NAME}");
            assert!(!exists(kernel, &device));

            bind_provider(kernel, provider_with_entities("unserved", ["other"]));
            assert!(!exists(kernel, &device));

            bind_provider(
                kernel,
                provider_with_entities(
                    "served",
                    [format!("{prefix}a"), PLATFORM_POWER_ENTITY.to_string()],
                ),
            );
            assert!(exists(kernel, &device));
            assert!(exists(kernel, &format!("{device}/{prefix}a")));
            assert!(exists(kernel, &format!("{device}/{PLATFORM_POWER_FILE}")));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn only_first_provider_is_served() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let prefix = PERF_STATE_FILE_PREFIX;
            bind_provider(kernel, provider_with_entities("first", [format!("{prefix}a")]));
            bind_provider(kernel, provider_with_entities("second", [format!("{prefix}b")]));

            let device = format!("devices/platform/{RESIDENCY_DEVICE_NAME}");
            assert!(exists(kernel, &format!("{device}/{prefix}a")));
            assert!(!exists(kernel, &format!("{device}/{prefix}b")));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn each_device_is_added_for_first_provider_serving_it() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let prefix = PERF_STATE_FILE_PREFIX;
            bind_provider(kernel, provider_with_entities("first", [format!("{prefix}a")]));
            bind_provider(
                kernel,
                provider_with_entities(
                    "second",
                    [format!("{prefix}b"), POWER_STATE_STATS_ENTITY.to_string()],
                ),
            );

            let residency_device = format!("devices/platform/{RESIDENCY_DEVICE_NAME}");
            assert!(exists(kernel, &format!("{residency_device}/{prefix}a")));
            assert!(!exists(kernel, &format!("{residency_device}/{prefix}b")));
            let stats_device = format!("devices/platform/{CAP_SYSFS_DEVICE_NAME}");
            assert!(exists(kernel, &format!("bus/platform/devices/{CAP_SYSFS_DEVICE_NAME}")));
            assert!(exists(kernel, &format!("{stats_device}/{POWER_STATE_STATS_FILE}")));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn read_returns_formatted_residency() {
        spawn_kernel_and_run_sync(|current_task| {
            let entities = [
                stats(
                    "lpcm_a",
                    vec![state(
                        /* id = */ 0, /* entry_count = */ 11,
                        /* total_time_ms = */ 2200,
                    )],
                ),
                stats(
                    "pwrblk_a",
                    vec![state(
                        /* id = */ 0, /* entry_count = */ 5,
                        /* total_time_ms = */ 40,
                    )],
                ),
                stats(
                    PLATFORM_POWER_ENTITY,
                    vec![state(
                        /* id = */ 0, /* entry_count = */ 3,
                        /* total_time_ms = */ 100,
                    )],
                ),
                stats(
                    "blockers_a",
                    vec![state(
                        /* id = */ 0, /* entry_count = */ 2,
                        /* total_time_ms = */ 30,
                    )],
                ),
                stats(
                    POWER_STATE_STATS_ENTITY,
                    vec![state(
                        /* id = */ 0, /* entry_count = */ 6,
                        /* total_time_ms = */ 250,
                    )],
                ),
            ];
            let provider = Arc::new(fake_provider("test", entities.to_vec()));

            for entity in &entities {
                let spec = residency_file_spec(&entity.entity).expect("entity is served");
                let file = ResidencyFile {
                    entity: spec.entity,
                    format: spec.format,
                    provider: provider.clone(),
                };
                assert_eq!(
                    &*file.read(current_task).expect("read residency file"),
                    (spec.format)(entity).as_bytes(),
                    "{}",
                    entity.entity
                );
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn read_fails_when_entity_is_no_longer_reported() {
        spawn_kernel_and_run_sync(|current_task| {
            let (proxy, server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            let reports = vec![stats("b", vec![])];
            spawn_server(move || serve_provider(server.into_stream(), reports));
            let provider = Arc::new(PowerStatsProvider::new("test", ["a", "b"], proxy));
            let file = perf_file("a", provider);

            assert_eq!(file.read(current_task).map(|_| ()), error!(EIO));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn read_fails_when_provider_is_gone() {
        spawn_kernel_and_run_sync(|current_task| {
            let (proxy, server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            drop::<ServerEnd<_>>(server);
            let provider = Arc::new(PowerStatsProvider::new("test", ["a"], proxy));
            let file = perf_file("a", provider);

            assert_eq!(file.read(current_task).map(|_| ()), error!(EIO));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn read_fails_when_provider_does_not_respond() {
        spawn_kernel_and_run_sync(|current_task| {
            // Keep `_server` open without servicing requests so `GetStats` times out.
            let (proxy, _server) = create_sync_proxy::<fpowerstats::DeviceMarker>();
            let provider = Arc::new(PowerStatsProvider::new("test", ["a"], proxy));
            let file = perf_file("a", provider);

            assert_eq!(file.read(current_task).map(|_| ()), error!(EIO));
        })
        .await;
    }

    #[::fuchsia::test]
    fn performance_states_are_formatted_per_state() {
        let stats = stats(
            "a",
            vec![
                state(
                    /* id = */ 0, /* entry_count = */ 11, /* total_time_ms = */ 2200,
                ),
                state(/* id = */ 1, /* entry_count = */ 4, /* total_time_ms = */ 7),
            ],
        );

        assert_eq!(
            format_performance_states(&stats),
            "num_pf_states: 2\n\
             pf_state: 0\npf_entry_count: 11\npf_time_in_state: 2200000 (us)\n\n\
             pf_state: 1\npf_entry_count: 4\npf_time_in_state: 7000 (us)\n\n"
        );
    }

    #[::fuchsia::test]
    fn power_domain_states_are_formatted_per_state() {
        let off = fpowerstats::StateResidency {
            name: "STATE_OFF".to_string(),
            last_entry_timestamp_ms: 5,
            ..state(/* id = */ 0, /* entry_count = */ 3, /* total_time_ms = */ 12)
        };
        let on = fpowerstats::StateResidency {
            name: "STATE_ON".to_string(),
            last_entry_timestamp_ms: 9,
            ..state(/* id = */ 1, /* entry_count = */ 4, /* total_time_ms = */ 7)
        };
        let stats = stats("a", vec![off, on]);

        assert_eq!(
            format_power_domain_states(&stats),
            "\nSSWRP lpb\n\n\
             power_state: STATE_OFF\nentry_count: 3\ntime_in_state: 12000 (us)\n\
             last_entry_timestamp: 5000\nlast_exit_timestamp: 0\n\
             power_state: STATE_ON\nentry_count: 4\ntime_in_state: 7000 (us)\n\
             last_entry_timestamp: 9000\nlast_exit_timestamp: 0\n\n"
        );
    }

    #[::fuchsia::test]
    fn platform_power_states_are_formatted_per_state() {
        let active = fpowerstats::StateResidency {
            name: "ACTIVE".to_string(),
            last_entry_timestamp_ms: 2,
            ..state(/* id = */ 0, /* entry_count = */ 3, /* total_time_ms = */ 100)
        };
        let stats = stats(
            "a",
            vec![active, state(/* id = */ 1, /* entry_count = */ 0, /* total_time_ms = */ 0)],
        );

        assert_eq!(
            format_platform_power_states(&stats),
            "power_state: ACTIVE\nentry_count: 3\ntime_in_state_ms: 100\n\
             last_entry_timestamp_ticks: 76800\nlast_exit_timestamp_ticks: 0\n\n\
             power_state: 1\nentry_count: 0\ntime_in_state_ms: 0\n\
             last_entry_timestamp_ticks: 0\nlast_exit_timestamp_ticks: 0\n\n"
        );
    }

    #[::fuchsia::test]
    fn power_state_blockers_are_formatted_per_blocker() {
        let kernel = fpowerstats::StateResidency {
            name: "Kernel blocker".to_string(),
            last_entry_timestamp_ms: 2,
            ..state(/* id = */ 0, /* entry_count = */ 5, /* total_time_ms = */ 0)
        };
        let hardware = fpowerstats::StateResidency {
            name: "Hardware blocker".to_string(),
            last_entry_timestamp_ms: 3,
            ..state(/* id = */ 1, /* entry_count = */ 4, /* total_time_ms = */ 120)
        };
        let stats = stats("blockers_deep_sleep", vec![kernel, hardware]);

        assert_eq!(
            format_power_state_blockers(&stats),
            "\n[DEEP_SLEEP]\n\n\
             Blocker: Kernel blocker\nBlocked Count: 5\n\
             Last Blocked Timestamp (ticks): 76800\nBlocked Time (ms): 0\n\n\
             Blocker: Hardware blocker\nBlocked Count: 4\n\
             Last Blocked Timestamp (ticks): 115200\nBlocked Time (ms): 120\n\n"
        );
    }

    #[::fuchsia::test]
    fn power_state_stats_are_formatted_per_state() {
        let cluster_off = fpowerstats::StateResidency {
            name: "Cluster 0 off".to_string(),
            last_entry_timestamp_ms: 2,
            ..state(/* id = */ 0, /* entry_count = */ 6, /* total_time_ms = */ 250)
        };
        let suspend = fpowerstats::StateResidency {
            name: "Suspend".to_string(),
            ..state(/* id = */ 1, /* entry_count = */ 1, /* total_time_ms = */ 10)
        };
        let stats = stats(POWER_STATE_STATS_ENTITY, vec![cluster_off, suspend]);

        assert_eq!(
            format_power_state_stats(&stats),
            "---\nstate: Cluster 0 off\nlast_entry_ts_ticks: 76800\nlast_exit_ts_ticks: 0\n\
             total_state_residency_ms: 250\nlast_state_entry_counts: 6\n\
             ---\nstate: Suspend\nlast_entry_ts_ticks: 0\nlast_exit_ts_ticks: 0\n\
             total_state_residency_ms: 10\nlast_state_entry_counts: 1\n"
        );
    }

    #[::fuchsia::test]
    fn dvfs_stats_are_formatted_per_level() {
        let fast = fpowerstats::StateResidency {
            last_entry_timestamp_ms: 3,
            ..state(/* id = */ 0, /* entry_count = */ 7, /* total_time_ms = */ 120)
        };
        let stats = stats(
            "cap_dvfs_clk_lit",
            vec![fast, state(/* id = */ 1, /* entry_count = */ 0, /* total_time_ms = */ 0)],
        );

        assert_eq!(
            format_dvfs_stats(&stats),
            "---\nclock_domain: clk_lit\nper_op_level_stats: \n\
             \x20 - oplevel: 0\n    entry_count: 7\n    total_residency_ms: 120\n\
             \x20   last_entry_timestamp_ticks: 115200\n\
             \x20 - oplevel: 1\n    entry_count: 0\n    total_residency_ms: 0\n\
             \x20   last_entry_timestamp_ticks: 0\n"
        );
    }

    #[::fuchsia::test]
    fn residency_with_no_states() {
        let stats = stats("a", vec![]);

        assert_eq!(format_performance_states(&stats), "num_pf_states: 0\n");
        assert_eq!(format_power_domain_states(&stats), "\nSSWRP lpb\n\n\n");
        assert_eq!(format_platform_power_states(&stats), "");
        assert_eq!(format_power_state_stats(&stats), "");
        assert_eq!(format_dvfs_stats(&stats), "---\nclock_domain: a\nper_op_level_stats: \n");
        assert_eq!(format_power_state_blockers(&stats), "\n[A]\n\n");
    }

    #[::fuchsia::test]
    fn states_with_unsafe_names_are_left_out() {
        let unsafe_state = fpowerstats::StateResidency {
            name: "A\nentry_count: 9".to_string(),
            ..state(/* id = */ 0, /* entry_count = */ 1, /* total_time_ms = */ 1)
        };
        let stats = stats(
            "a",
            vec![unsafe_state, state(/* id = */ 1, /* entry_count = */ 2, /* total_time_ms = */ 2)],
        );

        assert_eq!(
            format_power_domain_states(&stats),
            "\nSSWRP lpb\n\n\
             power_state: 1\nentry_count: 2\ntime_in_state: 2000 (us)\n\
             last_entry_timestamp: 0\nlast_exit_timestamp: 0\n\n"
        );
        assert_eq!(
            format_platform_power_states(&stats),
            "power_state: 1\nentry_count: 2\ntime_in_state_ms: 2\n\
             last_entry_timestamp_ticks: 0\nlast_exit_timestamp_ticks: 0\n\n"
        );
        assert_eq!(
            format_power_state_stats(&stats),
            "---\nstate: 1\nlast_entry_ts_ticks: 0\nlast_exit_ts_ticks: 0\n\
             total_state_residency_ms: 2\nlast_state_entry_counts: 2\n"
        );
        assert_eq!(
            format_power_state_blockers(&stats),
            "\n[A]\n\nBlocker: 1\nBlocked Count: 2\n\
             Last Blocked Timestamp (ticks): 0\nBlocked Time (ms): 2\n\n"
        );
    }

    #[::fuchsia::test]
    fn only_served_entities_with_safe_names_are_mapped_to_files() {
        for prefix in
            [PERF_STATE_FILE_PREFIX, POWER_DOMAIN_FILE_PREFIX, POWER_STATE_BLOCKERS_FILE_PREFIX]
        {
            let names = [
                format!("{prefix}a"),
                format!("{prefix}b_1"),
                PLATFORM_POWER_ENTITY.to_string(),
                POWER_STATE_STATS_ENTITY.to_string(),
                "other".to_string(),
                prefix.to_string(),
                format!("{prefix}../x"),
                format!("{prefix}a/b"),
                prefix.to_uppercase() + "A",
                format!("x{prefix}a"),
                format!("{PLATFORM_POWER_ENTITY}_x"),
                format!("{POWER_STATE_STATS_ENTITY}_x"),
            ];

            let served: Vec<_> = names
                .into_iter()
                .filter_map(|name| residency_file_spec(&name).map(|spec| (spec.device, spec.file)))
                .collect();
            assert_eq!(
                served,
                vec![
                    (RESIDENCY_DEVICE_NAME, format!("{prefix}a")),
                    (RESIDENCY_DEVICE_NAME, format!("{prefix}b_1")),
                    (RESIDENCY_DEVICE_NAME, PLATFORM_POWER_FILE.to_string()),
                    (CAP_SYSFS_DEVICE_NAME, POWER_STATE_STATS_FILE.to_string()),
                ]
            );
        }
    }

    #[::fuchsia::test]
    fn dvfs_entities_are_mapped_to_cap_sysfs_files() {
        let names = [
            "cap_dvfs_clk_lit",
            "cap_dvfs_clk_mid0",
            "cap_dvfs_",
            "cap_dvfs_../x",
            "cap_dvfs_a-b",
            "xcap_dvfs_a",
            // DVFS entities of other components are not served under cap_sysfs.
            "dvfs_gpu",
        ];

        let served: Vec<_> = names
            .into_iter()
            .filter_map(|name| residency_file_spec(name).map(|spec| (spec.device, spec.file)))
            .collect();
        assert_eq!(
            served,
            vec![
                (CAP_SYSFS_DEVICE_NAME, "dvfs_stats_clk_lit".to_string()),
                (CAP_SYSFS_DEVICE_NAME, "dvfs_stats_clk_mid0".to_string()),
            ]
        );
    }

    #[::fuchsia::test]
    async fn dsu_devfreq_device_is_added_with_dsu_entity() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            bind_provider(kernel, provider_with_entities("default", [DSU_DVFS_ENTITY]));

            let device = format!("devices/platform/{DSU_DEVFREQ_DEVICE_NAME}");
            assert!(exists(kernel, &format!("bus/platform/devices/{DSU_DEVFREQ_DEVICE_NAME}")));
            assert!(exists(kernel, &format!("{device}/uevent")));
            assert!(exists(
                kernel,
                &format!("{device}/devfreq/{DSU_DEVFREQ_DEVICE_NAME}/available_frequencies")
            ));
            assert!(exists(kernel, &format!("class/devfreq/{DSU_DEVFREQ_DEVICE_NAME}")));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn no_dsu_devfreq_device_without_dsu_entity() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            bind_provider(kernel, provider_with_entities("default", ["cap_dvfs_clk_lit"]));

            assert!(exists(kernel, &format!("devices/platform/{CAP_SYSFS_DEVICE_NAME}")));
            assert!(!exists(kernel, &format!("devices/platform/{DSU_DEVFREQ_DEVICE_NAME}")));

            bind_provider(kernel, provider_with_entities("second", [DSU_DVFS_ENTITY]));
            assert!(!exists(kernel, &format!("devices/platform/{DSU_DEVFREQ_DEVICE_NAME}")));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn dsu_devfreq_device_is_added_once() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            bind_provider(kernel, provider_with_entities("first", [DSU_DVFS_ENTITY]));
            bind_provider(kernel, provider_with_entities("second", [DSU_DVFS_ENTITY]));

            assert!(exists(kernel, &format!("devices/platform/{DSU_DEVFREQ_DEVICE_NAME}")));
        })
        .await;
    }

    #[::fuchsia::test]
    fn dsu_available_frequencies_are_listed_from_slowest() {
        assert!(DSU_AVAILABLE_FREQUENCIES_HZ.is_sorted_by(|a, b| a < b));
        assert_eq!(
            dsu_available_frequencies(),
            "115000000 268000000 400000000 537000000 691000000 768000000 844000000 960000000 \
             1056000000 1132000000 1228000000 1324000000 1420000000 1516000000 1612000000 \
             1728000000 1843000000 1920000000 1996000000 2092000000 2188000000\n"
        );
    }
}
