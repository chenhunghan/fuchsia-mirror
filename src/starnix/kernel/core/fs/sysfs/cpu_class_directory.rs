// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fs::sysfs::build_device_directory;
use crate::task::{CurrentTask, Kernel};
use crate::vfs::pseudo::simple_directory::SimpleDirectoryMutator;
use crate::vfs::pseudo::simple_file::{
    BytesFile, BytesFileOps, SimpleFileNode, parse_unsigned_file,
};
use crate::vfs::pseudo::stub_empty_file::StubEmptyFile;
use crate::vfs::{FsNodeOps, FsString};
use fidl_fuchsia_hardware_cpu_ctrl as fcpuctrl;
use fidl_fuchsia_power_cpu as fcpu;
use fuchsia_component::client::connect_to_protocol_sync;
use itertools::Itertools;
use starnix_logging::{bug_ref, log_warn};
use starnix_sync::{CpuFreqProxyCacheLock, LockDepMutex};
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::{errno, error, from_status_like_fdio};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use zx;

#[derive(Clone)]
struct CpuFreqDomains(Arc<[Arc<CpuFreqDomain>]>);

/// Returns the per-domain CPU frequency state shared across sysfs `cpufreq` nodes and thermal
/// cooling devices for `kernel`.
pub fn get_cpu_freq_domains(kernel: &Kernel) -> Arc<[Arc<CpuFreqDomain>]> {
    kernel
        .expando
        .get_or_init(|| {
            let DiscoveredCpuDomains { domains, has_domain_controller } = get_cpu_domains();
            CpuFreqDomains(
                domains
                    .iter()
                    .map(|domain| Arc::new(CpuFreqDomain::new(domain, has_domain_controller)))
                    .collect(),
            )
        })
        .0
        .clone()
}

/// Registers `/sys/devices/system/cpu` and the per-core `cpuN` bus devices beneath it.
pub fn register_cpu_devices(kernel: &Kernel) {
    // Each domain is wrapped once and shared across `cpuN/cpufreq`, `cpufreq/policyN`, and
    // thermal `cooling_device*` nodes so all voters aggregate on the same domain state.
    let cpu_domains = get_cpu_freq_domains(kernel);

    let mut core_to_domain_map: Vec<(u64, &Arc<CpuFreqDomain>)> = cpu_domains
        .iter()
        .flat_map(|domain| domain.core_ids.iter().map(move |id| (*id, domain)))
        .collect();
    core_to_domain_map.sort_by_key(|(id, _)| *id);
    core_to_domain_map.dedup_by_key(|(id, _)| *id);

    let core_count = core_to_domain_map.len();
    let registry = &kernel.device_registry;
    let system_device = registry.objects.system_device();
    let cpu_root =
        registry.add_subsystemless_device("cpu".into(), Some(system_device), |device, dir| {
            build_device_directory(device, dir);
            build_cpu_subsystem_directory(dir, &cpu_domains, core_count);
        });

    let cpu_bus = registry.objects.get_or_create_bus("cpu".into());
    for (core_id, domain) in &core_to_domain_map {
        let name: FsString = format!("cpu{}", core_id).into();
        registry.add_bus_device(
            name.as_ref(),
            Some(cpu_root.clone()),
            cpu_bus.clone(),
            |device, dir| {
                build_device_directory(device, dir);
                build_cpu_directory(dir, *core_id, domain);
            },
        );
    }
}

/// Populates the subsystem-wide attributes in `/sys/devices/system/cpu`.
fn build_cpu_subsystem_directory(
    dir: &SimpleDirectoryMutator,
    cpu_domains: &[Arc<CpuFreqDomain>],
    core_count: usize,
) {
    dir.entry(
        "online",
        BytesFile::new_node(format!("0-{}\n", core_count.saturating_sub(1)).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "possible",
        BytesFile::new_node(format!("0-{}\n", core_count.saturating_sub(1)).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.subdir("vulnerabilities", 0o755, |dir| {
        for (name, contents) in VULNERABILITIES {
            let contents = contents.to_string();
            dir.entry(name, BytesFile::new_node(contents.into_bytes()), mode!(IFREG, 0o444));
        }
    });
    dir.subdir("cpufreq", 0o755, |dir| {
        for domain in cpu_domains.iter() {
            let min_core_id = domain.core_ids.iter().min().expect("core_ids is empty");
            let name = format!("policy{}", min_core_id);
            dir.subdir(&name, 0o755, |dir| build_cpufreq_directory(dir, domain));
        }
    });
    dir.subdir("soc", 0o755, |dir| {
        dir.subdir("0", 0o755, |dir| {
            dir.entry(
                "machine",
                StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                mode!(IFREG, 0o444),
            );
        });
    });
}

struct DiscoveredCpuDomains {
    domains: Vec<fcpu::DomainInfo>,
    has_domain_controller: bool,
}

/// Retrieves CPU topology and domain information with a tiered fallback approach.
///
/// 1. Get CPU domains from the `fuchsia.power.cpu.DomainController` FIDL protocol.
/// 2. If that fails, use the `fuchsia.hardware.cpu.ctrl.Service` FIDL service to connect to
///    individual CPU control devices.
/// 3. If that fails, get CPU information from the kernel directly. In this case, no CPU control
///    or topological information is available, so only a limited set of sysfs entries
///    will be populated.
fn get_cpu_domains() -> DiscoveredCpuDomains {
    // Tier 1: Try DomainController
    if let Ok(domain_controller) = connect_to_protocol_sync::<fcpu::DomainControllerMarker>() {
        if let Ok(mut domains) = domain_controller.list_domains(zx::MonotonicInstant::INFINITE) {
            // Remove any domains without an ID or empty core_ids.
            domains
                .retain(|d| d.id.is_some() && d.core_ids.as_ref().map_or(false, |c| !c.is_empty()));
            if !domains.is_empty() {
                return DiscoveredCpuDomains { domains, has_domain_controller: true };
            }
        }
    }

    log_warn!(
        "Could not retrieve CPU domains from fuchsia.power.cpu.DomainController, using CPU control devices instead."
    );

    // Tier 2: Try cpu.ctrl devices
    if let Ok(proxies) = connect_to_cpu_devices() {
        let mut domains = Vec::new();

        for proxy in proxies {
            let cpu_count = match proxy.get_num_logical_cores(zx::MonotonicInstant::INFINITE) {
                Ok(count) => count,
                Err(e) => {
                    log_warn!("get_num_logical_cores returned error: {}", e);
                    continue;
                }
            };
            let domain_id = match proxy.get_domain_id(zx::MonotonicInstant::INFINITE) {
                Ok(id) => id as u64,
                Err(e) => {
                    log_warn!("get_domain_id returned error: {}", e);
                    continue;
                }
            };

            let mut core_ids = Vec::with_capacity(cpu_count as usize);
            let mut get_core_failed = false;
            for i in 0..cpu_count {
                let core_id = match proxy.get_logical_core_id(i, zx::MonotonicInstant::INFINITE) {
                    Ok(id) => id,
                    Err(e) => {
                        log_warn!("get_logical_core_id error in domain {}: {}", domain_id, e);
                        get_core_failed = true;
                        break;
                    }
                };
                core_ids.push(core_id);
            }
            if get_core_failed || core_ids.is_empty() {
                log_warn!("get_logical_core_id failed in domain {}, skipping", domain_id);
                continue;
            }

            let available_frequencies_hz =
                match proxy.get_operating_point_count(zx::MonotonicInstant::INFINITE) {
                    Ok(Ok(count)) => {
                        let mut freqs = Vec::with_capacity(count as usize);
                        for i in 0..count {
                            if let Ok(Ok(info)) =
                                proxy.get_operating_point_info(i, zx::MonotonicInstant::INFINITE)
                            {
                                if info.frequency_hz > 0 {
                                    freqs.push(info.frequency_hz as u64);
                                }
                            }
                        }
                        freqs.sort();
                        freqs.dedup();
                        Some(freqs)
                    }
                    _ => None,
                };

            domains.push(fcpu::DomainInfo {
                id: Some(domain_id),
                core_ids: Some(core_ids),
                available_frequencies_hz,
                ..Default::default()
            });
        }

        if !domains.is_empty() {
            return DiscoveredCpuDomains { domains, has_domain_controller: false };
        }
    }

    log_warn!(
        "Could not connect to CPU control devices, using default domain info from kernel CPU count."
    );

    // Tier 3: Fallback to kernel CPU count
    let cpu_count = zx::system_get_num_cpus();
    DiscoveredCpuDomains {
        domains: vec![fcpu::DomainInfo {
            id: Some(0),
            core_ids: Some((0..cpu_count as u64).collect()),
            available_frequencies_hz: None,
            name: None,
            ..Default::default()
        }],
        has_domain_controller: false,
    }
}

fn hz_to_khz(hz: u64) -> u64 {
    hz / 1000
}

/// The only CPU frequency governor Starnix reports.
///
/// Frequency selection is not user-tunable on Fuchsia, so `scaling_governor` is a fixed value
/// rather than a real choice.
const SCALING_GOVERNOR: &str = "performance";

/// Formats a frequency for a sysfs node, leaving the value empty when the domain reports none.
fn format_freq_khz(khz: Option<u64>) -> String {
    format!("{}\n", khz.map(|f| f.to_string()).unwrap_or_default())
}

fn build_cpu_directory(dir: &SimpleDirectoryMutator, core_id: u64, domain: &Arc<CpuFreqDomain>) {
    let cluster_id = domain.domain_id;

    dir.entry(
        "cpu_capacity",
        StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
        mode!(IFREG, 0o444),
    );
    dir.subdir("cpuidle", 0o755, |dir| {
        build_cpuidle_directory(dir, core_id);
    });
    dir.subdir("cpufreq", 0o755, |dir| {
        build_cpufreq_directory(dir, domain);
    });
    dir.subdir("topology", 0o755, |dir| {
        dir.entry(
            "cluster_id",
            BytesFile::new_node(format!("{cluster_id}\n").into_bytes()),
            mode!(IFREG, 0o444),
        );
        dir.entry(
            "physical_package_id",
            BytesFile::new_node(format!("{cluster_id}\n").into_bytes()),
            mode!(IFREG, 0o444),
        );
    });
}

fn build_cpuidle_directory(dir: &SimpleDirectoryMutator, core_id: u64) {
    // TODO(https://fxbug.dev/560171700): `fuchsia.kernel.Stats` (`PerCpuStats`) currently
    // reports a single aggregate `idle_time` per CPU core rather than per-C-state residencies.
    // Until Zircon exposes differentiated C-state statistics, attribute all aggregate idle
    // time to `state0` ("C1") and report 0 for `state1` ("C2") to avoid double-counting.
    dir.subdir("state0", 0o755, |dir| {
        dir.entry("name", BytesFile::new_node(b"C1\n".to_vec()), mode!(IFREG, 0o444));
        dir.entry("desc", BytesFile::new_node(b"idle\n".to_vec()), mode!(IFREG, 0o444));
        dir.entry(
            "time",
            BytesFile::new_node(CpuIdleStatFile { core_id, stat_type: CpuIdleStatType::Time }),
            mode!(IFREG, 0o444),
        );
        dir.entry(
            "usage",
            BytesFile::new_node(CpuIdleStatFile { core_id, stat_type: CpuIdleStatType::Usage }),
            mode!(IFREG, 0o444),
        );
    });
    dir.subdir("state1", 0o755, |dir| {
        dir.entry("name", BytesFile::new_node(b"C2\n".to_vec()), mode!(IFREG, 0o444));
        dir.entry("desc", BytesFile::new_node(b"deep idle\n".to_vec()), mode!(IFREG, 0o444));
        dir.entry("time", BytesFile::new_node(b"0\n".to_vec()), mode!(IFREG, 0o444));
        dir.entry("usage", BytesFile::new_node(b"0\n".to_vec()), mode!(IFREG, 0o444));
    });
}

/// Max-frequency voter nodes under `cpufreq/vote_manager/`.
const VOTE_MANAGER_MAX_FREQ_NODES: &[&str] =
    &["powerhint_max_freq", "thermal_max_freq", "soft_max_freq", "debug_max_freq"];

/// Min-frequency voter nodes under `cpufreq/vote_manager/`.
const VOTE_MANAGER_MIN_FREQ_NODES: &[&str] =
    &["powerhint_min_freq", "soft_min_freq", "debug_min_freq"];

fn add_max_freq_entry(
    dir: &SimpleDirectoryMutator,
    name: &'static str,
    domain: &Arc<CpuFreqDomain>,
) {
    // A domain that reports no operating points cannot be capped, so fall back to an inert node
    // rather than one that would fail every write.
    if domain.is_tunable() {
        dir.entry(name, ScalingMaxFreqFile::new_node(domain, name), mode!(IFREG, 0o644));
    } else {
        dir.entry(
            name,
            InertFile::new_node(format_freq_khz(domain.highest_khz())),
            mode!(IFREG, 0o644),
        );
    }
}

fn build_cpufreq_directory(dir: &SimpleDirectoryMutator, domain: &Arc<CpuFreqDomain>) {
    dir.subdir("stats", 0o755, |dir| {
        dir.entry("reset", CpuFreqStatsResetFile::new_node(), mode!(IFREG, 0o200));
        dir.entry(
            "time_in_state",
            StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
            mode!(IFREG, 0o444),
        );
    });

    // Android Power HAL and thermal/debug tooling vote through `cpufreq/vote_manager/*`. Each
    // max-frequency node (including `scaling_max_freq`) registers its own persistent vote in
    // `CpuFreqDomain`, which enforces the most restrictive ceiling across all active voters.
    dir.subdir("vote_manager", 0o755, |dir| {
        for name in VOTE_MANAGER_MAX_FREQ_NODES {
            add_max_freq_entry(dir, name, domain);
        }
        for name in VOTE_MANAGER_MIN_FREQ_NODES {
            dir.entry(
                name,
                InertFile::new_node(format_freq_khz(domain.lowest_khz())),
                mode!(IFREG, 0o644),
            );
        }
    });

    let related_cpus_str = format!("{}\n", domain.core_ids.iter().sorted().join(" "));
    dir.entry(
        "related_cpus",
        BytesFile::new_node(related_cpus_str.into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "scaling_cur_freq",
        create_scaling_cur_freq_file(domain.domain_id),
        mode!(IFREG, 0o444),
    );

    // Fuchsia has no per-domain minimum frequency control yet (b/564595245), so
    // `scaling_min_freq` accepts writes and discards them. It reads back the domain's lowest
    // frequency, which is the value an unconstrained Linux policy reports.
    dir.entry(
        "scaling_min_freq",
        InertFile::new_node(format_freq_khz(domain.lowest_khz())),
        mode!(IFREG, 0o644),
    );

    add_max_freq_entry(dir, "scaling_max_freq", domain);

    dir.entry(
        "scaling_available_frequencies",
        BytesFile::new_node((domain.sorted_khz.iter().join(" ") + "\n").into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "scaling_available_governors",
        BytesFile::new_node(format!("{SCALING_GOVERNOR}\n").into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "scaling_governor",
        InertFile::new_node(format!("{SCALING_GOVERNOR}\n")),
        mode!(IFREG, 0o644),
    );
    dir.entry(
        "cpuinfo_min_freq",
        BytesFile::new_node(format_freq_khz(domain.lowest_khz()).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "cpuinfo_max_freq",
        BytesFile::new_node(format_freq_khz(domain.highest_khz()).into_bytes()),
        mode!(IFREG, 0o444),
    );
}

const VULNERABILITIES: &[(&str, &str)] = &[
    ("gather_data_sampling", "Not affected\n"),
    ("itlb_multihit", "Not affected\n"),
    ("l1tf", "Not affected\n"),
    ("mds", "Not affected\n"),
    ("meltdown", "Not affected\n"),
    ("mmio_stale_data", "Not affected\n"),
    ("retbleed", "Not affected\n"),
    ("spec_rstack_overflow", "Not affected\n"),
    ("spec_store_bypass", "Not affected\n"),
    ("spectre_v1", "Not affected\n"),
    ("spectre_v2", "Not affected\n"),
    ("srbds", "Not affected\n"),
    ("tsx_async_abort", "Not affected\n"),
];

struct CpuFreqStatsResetFile {}

impl CpuFreqStatsResetFile {
    pub fn new_node() -> impl FsNodeOps {
        BytesFile::new_node(Self {})
    }
}

impl BytesFileOps for CpuFreqStatsResetFile {
    // Currently a no-op. The value written to this node does not matter.
    fn write(&self, _current_task: &CurrentTask, _data: Vec<u8>) -> Result<(), Errno> {
        Ok(())
    }
}

struct CpuFreqDomainState {
    /// Active ceiling votes keyed by voter name (`voter -> opp_index`), aggregated using Linux
    /// `freq_qos` / `vote_manager` semantics (the lowest maximum frequency wins).
    votes: HashMap<&'static str, u64>,

    /// The effective ceiling (`Some(opp_index)` or `None` when uncapped) last applied to
    /// `DomainController`, or `None` when invalidated by an error.
    applied_opp: Option<Option<u64>>,

    /// Lazily established connection to the server, dropped if a call fails at the transport
    /// level so that the next caller reconnects.
    domain_controller: Option<fcpu::DomainControllerSynchronousProxy>,
}

impl Default for CpuFreqDomainState {
    fn default() -> Self {
        Self { votes: HashMap::new(), applied_opp: Some(None), domain_controller: None }
    }
}

/// Per-domain state shared by a domain's cpufreq nodes and by its corresponding thermal
/// `cooling_device`.
pub struct CpuFreqDomain {
    domain_id: u64,

    /// Optional name of the domain/cluster from `DomainInfo.name`.
    name: Option<String>,

    /// The IDs of the CPU cores that are part of this domain.
    core_ids: Vec<u64>,

    /// Available frequencies in Hz, in `DomainInfo.available_frequencies_hz` order.
    ///
    /// This is operating point order: `DomainController.SetMaxFrequency` takes an index into
    /// this list. It is deliberately kept apart from `sorted_khz` below, which is what
    /// `scaling_available_frequencies` reports, because sorting reorders the entries. The two
    /// lists are not positionally related, so a frequency must be matched by value to find its
    /// operating point.
    opps_hz: Vec<u64>,

    /// `opps_hz` in kHz, sorted ascending and deduplicated.
    sorted_khz: Vec<u64>,

    /// Whether `DomainController` is serving this domain (as opposed to the `cpu.ctrl` or
    /// kernel-CPU-count fallbacks).
    has_domain_controller: bool,

    state: LockDepMutex<CpuFreqDomainState, CpuFreqProxyCacheLock>,
}

impl CpuFreqDomain {
    fn new(domain: &fcpu::DomainInfo, has_domain_controller: bool) -> Self {
        let opps_hz = domain.available_frequencies_hz.clone().unwrap_or_default();
        let sorted_khz = opps_hz.iter().map(|hz| hz_to_khz(*hz)).sorted().dedup().collect();
        Self {
            domain_id: domain.id.expect("id not available"),
            name: domain.name.clone(),
            core_ids: domain.core_ids.clone().expect("core_ids not available"),
            opps_hz,
            sorted_khz,
            has_domain_controller,
            state: LockDepMutex::default(),
        }
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn opp_count(&self) -> usize {
        self.opps_hz.len()
    }

    pub fn get_vote(&self, voter: &'static str) -> Option<u64> {
        self.state.lock().votes.get(voter).copied()
    }

    /// Returns `voter`'s active ceiling in kHz, or the domain's highest frequency when `voter` has
    /// no active cap.
    fn voter_frequency_khz(&self, voter: &'static str) -> Option<u64> {
        self.get_vote(voter)
            .and_then(|index| self.opps_hz.get(index as usize))
            .map(|&hz| hz_to_khz(hz))
            .or_else(|| self.highest_khz())
    }

    /// Whether the domain is backed by `DomainController` and reports operating points, and can
    /// therefore have its maximum frequency controlled.
    pub fn is_tunable(&self) -> bool {
        self.has_domain_controller && !self.opps_hz.is_empty()
    }

    fn lowest_khz(&self) -> Option<u64> {
        self.sorted_khz.first().copied()
    }

    fn highest_khz(&self) -> Option<u64> {
        self.sorted_khz.last().copied()
    }

    /// Maps a frequency in kHz to the operating point that caps the domain there.
    ///
    /// Returns `None` when the request is at or above the domain's highest operating point,
    /// meaning "no cap". A client releases a cap by writing a sentinel far above any real
    /// frequency, which lands here.
    ///
    /// Otherwise the request is snapped down to the highest operating point that does not
    /// exceed it, or to the lowest operating point if it undershoots them all.
    fn opp_index_for_khz(&self, khz: u64) -> Option<u64> {
        if khz >= self.highest_khz()? {
            return None;
        }
        self.opps_hz
            .iter()
            .enumerate()
            .filter(|(_, hz)| hz_to_khz(**hz) <= khz)
            .max_by_key(|(_, hz)| **hz)
            .or_else(|| self.opps_hz.iter().enumerate().min_by_key(|(_, hz)| **hz))
            .map(|(index, _)| index as u64)
    }

    /// Updates `voter`'s ceiling vote (`Some(opp_index)` or `None` to release) and applies the
    /// most restrictive ceiling across all active voters on this domain.
    pub fn update_vote(&self, voter: &'static str, opp_index: Option<u64>) -> Result<(), Errno> {
        if let Some(index) = opp_index {
            if index as usize >= self.opps_hz.len() {
                return error!(EINVAL);
            }
        }
        let mut state = self.state.lock();
        let previous = match opp_index {
            Some(index) => state.votes.insert(voter, index),
            None => state.votes.remove(voter),
        };
        let result = self.apply_effective_vote(&mut state);
        if result.is_err() {
            match previous {
                Some(prev) => {
                    state.votes.insert(voter, prev);
                }
                None => {
                    state.votes.remove(voter);
                }
            }
        }
        result
    }

    fn apply_effective_vote(&self, state: &mut CpuFreqDomainState) -> Result<(), Errno> {
        let effective_opp = state
            .votes
            .values()
            .copied()
            .min_by_key(|&index| (self.opps_hz[index as usize], std::cmp::Reverse(index)));
        if state.applied_opp == Some(effective_opp) {
            return Ok(());
        }
        state.applied_opp = None;
        match effective_opp {
            Some(index) => self.set_max_frequency(state, index)?,
            None => self.clear_max_frequency(state)?,
        }
        state.applied_opp = Some(effective_opp);
        Ok(())
    }

    /// Runs `call` against a lazily connected `DomainController`.
    ///
    /// A transport failure invalidates the cached connection and `applied_opp`; a protocol-level
    /// error leaves the connection in place for the caller to interpret.
    fn with_domain_controller<T, E: std::fmt::Debug>(
        &self,
        state: &mut CpuFreqDomainState,
        call: impl FnOnce(&fcpu::DomainControllerSynchronousProxy) -> Result<T, E>,
    ) -> Result<T, Errno> {
        if state.domain_controller.is_none() {
            state.domain_controller =
                Some(connect_to_protocol_sync::<fcpu::DomainControllerMarker>().map_err(|e| {
                    log_warn!("Failed to connect to DomainController: {:?}", e);
                    errno!(ENODEV)
                })?);
        }
        match call(state.domain_controller.as_ref().expect("connection was just established")) {
            Ok(value) => Ok(value),
            Err(e) => {
                log_warn!("DomainController call failed for domain {}: {:?}", self.domain_id, e);
                state.domain_controller = None;
                state.applied_opp = None;
                error!(EIO)
            }
        }
    }

    /// Reads back the frequency in kHz that the domain is currently capped to.
    fn max_frequency_khz(&self) -> Result<u64, Errno> {
        let mut state = self.state.lock();
        let index = self
            .with_domain_controller(&mut state, |dc| {
                dc.get_max_frequency(self.domain_id, zx::MonotonicInstant::INFINITE)
            })?
            .map_err(|e| {
                log_warn!("get_max_frequency failed for domain {}: {:?}", self.domain_id, e);
                errno!(EINVAL)
            })?;
        let hz = self.opps_hz.get(index as usize).ok_or_else(|| errno!(EINVAL))?;
        Ok(hz_to_khz(*hz))
    }

    fn set_max_frequency(&self, state: &mut CpuFreqDomainState, index: u64) -> Result<(), Errno> {
        self.with_domain_controller(state, |dc| {
            dc.set_max_frequency(self.domain_id, index, zx::MonotonicInstant::INFINITE)
        })?
        .map_err(|e| {
            log_warn!("set_max_frequency failed for domain {}: {:?}", self.domain_id, e);
            errno!(EINVAL)
        })
    }

    fn clear_max_frequency(&self, state: &mut CpuFreqDomainState) -> Result<(), Errno> {
        self.with_domain_controller(state, |dc| {
            dc.clear_max_frequency(self.domain_id, zx::MonotonicInstant::INFINITE)
        })?
        .map_err(|e| {
            log_warn!("clear_max_frequency failed for domain {}: {:?}", self.domain_id, e);
            errno!(EINVAL)
        })
    }
}

/// A writable max-frequency node (`scaling_max_freq` and `vote_manager/*_max_freq`), backed by
/// `fuchsia.power.cpu/DomainController`.
///
/// Votes are keyed by the sysfs node identity (`voter`) in `CpuFreqDomain` and persist across
/// `close()` until overwritten or cleared with a value at or above the domain maximum.
struct ScalingMaxFreqFile {
    domain: Arc<CpuFreqDomain>,
    voter: &'static str,
}

impl ScalingMaxFreqFile {
    fn new_node(domain: &Arc<CpuFreqDomain>, voter: &'static str) -> impl FsNodeOps {
        BytesFile::new_node(Self { domain: domain.clone(), voter })
    }
}

impl BytesFileOps for ScalingMaxFreqFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let content = if self.voter == "scaling_max_freq" {
            format!("{}\n", self.domain.max_frequency_khz()?)
        } else {
            format_freq_khz(self.domain.voter_frequency_khz(self.voter))
        };
        Ok(content.into_bytes().into())
    }

    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let opp_index = self.domain.opp_index_for_khz(parse_unsigned_file::<u64>(&data)?);
        self.domain.update_vote(self.voter, opp_index)
    }
}

/// A sysfs node that accepts writes, discards them, and reads back a fixed value.
///
/// This is for cpufreq nodes that the Android Power HAL writes but that Fuchsia cannot act on.
/// Leaving such a node read-only is worse than making it inert: libperfmgr suppresses its
/// warning and its 500ms retry clamp only when `open()` fails with `ENOENT`, so a node that
/// exists but rejects writes produces log spam and a busy retry loop on every hint. Removing
/// the node instead would return `ENOENT` to every reader, not just to libperfmgr.
struct InertFile(Vec<u8>);

impl InertFile {
    fn new_node(contents: String) -> impl FsNodeOps {
        BytesFile::new_node(Self(contents.into_bytes()))
    }
}

impl BytesFileOps for InertFile {
    fn write(&self, _current_task: &CurrentTask, _data: Vec<u8>) -> Result<(), Errno> {
        Ok(())
    }

    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        Ok(self.0.as_slice().into())
    }
}

const CPU_DIRECTORY: &str = "/svc/fuchsia.hardware.cpu.ctrl.Service";

fn connect_to_cpu_devices() -> Result<Vec<fcpuctrl::DeviceSynchronousProxy>, Errno> {
    let dir = std::fs::read_dir(CPU_DIRECTORY).map_err(|_| errno!(EINVAL))?;

    let proxies: Vec<_> = dir
        .filter_map(|r| r.ok())
        .filter_map(|entry| {
            let path = entry.path().join("device").into_os_string().into_string().ok()?;
            let (client, server) = zx::Channel::create();
            fdio::service_connect(&path, server).ok()?;
            Some(fcpuctrl::DeviceSynchronousProxy::new(client))
        })
        .collect();

    if proxies.is_empty() { error!(ENOENT) } else { Ok(proxies) }
}

fn connect_to_cpu_device_by_domain_id(
    domain_id: u64,
) -> Result<fcpuctrl::DeviceSynchronousProxy, Errno> {
    let dir = std::fs::read_dir(CPU_DIRECTORY).map_err(|_| errno!(EINVAL))?;

    dir.filter_map(|r| r.ok())
        .find_map(|entry| {
            let path = entry.path().join("device").into_os_string().into_string().ok()?;
            let (client, server) = zx::Channel::create();
            fdio::service_connect(&path, server).ok()?;
            let proxy = fcpuctrl::DeviceSynchronousProxy::new(client);

            let dev_domain_id = proxy.get_domain_id(zx::MonotonicInstant::INFINITE).ok()?;
            if domain_id == dev_domain_id as u64 { Some(proxy) } else { None }
        })
        .ok_or_else(|| errno!(ENOENT))
}

fn create_scaling_cur_freq_file(domain_id: u64) -> impl FsNodeOps {
    let proxy_cache =
        LockDepMutex::<_, CpuFreqProxyCacheLock>::new(None::<fcpuctrl::DeviceSynchronousProxy>);
    SimpleFileNode::new(move |_| {
        let mut guard = proxy_cache.lock();
        if guard.is_none() {
            let proxy = connect_to_cpu_device_by_domain_id(domain_id)?;
            *guard = Some(proxy);
        }
        let proxy = guard.as_ref().expect("must have a valid proxy");
        let opp = match proxy.get_current_operating_point(zx::MonotonicInstant::INFINITE) {
            Ok(opp) => opp,
            Err(_) => {
                *guard = None;
                return error!(EINVAL);
            }
        };
        let info = match proxy.get_operating_point_info(opp, zx::MonotonicInstant::INFINITE) {
            Ok(info) => info,
            Err(_) => {
                *guard = None;
                return error!(EINVAL);
            }
        };
        let info = info.map_err(|e| from_status_like_fdio!(zx::Status::err_from_raw(e)))?;
        if info.frequency_hz <= 0 {
            return error!(EINVAL);
        }
        let freq_khz = hz_to_khz(info.frequency_hz as u64);
        Ok(BytesFile::new(format!("{}\n", freq_khz).into_bytes()))
    })
}

enum CpuIdleStatType {
    Time,
    Usage,
}

struct CpuIdleStatFile {
    core_id: u64,
    stat_type: CpuIdleStatType,
}

impl BytesFileOps for CpuIdleStatFile {
    fn read(&self, current_task: &CurrentTask) -> Result<std::borrow::Cow<'_, [u8]>, Errno> {
        let cpu_stats = current_task
            .kernel()
            .stats
            .get()
            .get_cpu_stats(zx::MonotonicInstant::INFINITE)
            .map_err(|_| errno!(EINVAL))?;
        let per_cpu_stats = cpu_stats.per_cpu_stats.ok_or_else(|| errno!(EINVAL))?;
        let per_cpu = per_cpu_stats.get(self.core_id as usize).ok_or_else(|| errno!(EINVAL))?;
        let value = match self.stat_type {
            CpuIdleStatType::Time => {
                // Zircon idle_time is in nanoseconds. Linux cpuidle time is in microseconds.
                per_cpu.idle_time.ok_or_else(|| errno!(EINVAL))? / 1000
            }
            CpuIdleStatType::Usage => per_cpu.reschedules.ok_or_else(|| errno!(EINVAL))? as i64,
        };
        Ok(format!("{value}\n").into_bytes().into())
    }
}
