// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Inspect properties and lazy counter recording for `ufs_rust`.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use fdf_component::Incoming;
use fidl::endpoints::ClientEnd;
use fidl_fuchsia_hardware_ufs as fufs;
use fidl_fuchsia_inspect as finspect;
use fuchsia_async as fasync;
use fuchsia_inspect::{self as inspect, HistogramProperty, Inspector, Property};
use inspect_runtime::PublishOptions;
use log::error;
use mmio::Mmio;

use crate::constants::{HIGHEST_ACTIVE_ICC_LEVEL, MAX_TRANSFER_BYTES, REQUEST_GAP_BUCKETS};
use crate::device_manager::{DeviceManager, UfsPowerMode};
use crate::power::{POWER_STEP_IDLE, PowerState};
use crate::scsi::StartStopPowerCondition;
use crate::transfer_request_processor::TransferRequestProcessor;

/// UniPro link state, reported as the `controller/LinkState` Inspect property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LinkState {
    Off = 0,
    Active = 1,
    Hibernate = 2,
}

/// Inspect tree name (`[name=ufs]root/ufs:*`) expected by Sampler and diagnostics selectors.
pub const UFS_INSPECT_TREE_NAME: &str = "ufs";

/// Sentinel stored in [`DeviceFlags::wb_dedicated_lun`] when WriteBooster uses a shared buffer or
/// is disabled, so flag updates are not filtered to a single dedicated LUN index.
const NO_DEDICATED_WB_LUN: u8 = u8::MAX;

/// Connects to `fuchsia.inspect.InspectSink` in `incoming` and publishes `inspector` under tree
/// name [`UFS_INSPECT_TREE_NAME`].
pub fn publish_ufs_inspect(
    incoming: &Incoming,
    inspector: &Inspector,
    scope: fasync::ScopeHandle,
) -> Result<(), zx::Status> {
    let client =
        incoming.connect_protocol::<ClientEnd<finspect::InspectSinkMarker>>().map_err(|err| {
            error!(err:%; "Error connecting to InspectSink");
            zx::Status::INTERNAL
        })?;
    publish_ufs_inspect_with_client(inspector, client, scope)
}

fn publish_ufs_inspect_with_client(
    inspector: &Inspector,
    client: ClientEnd<finspect::InspectSinkMarker>,
    scope: fasync::ScopeHandle,
) -> Result<(), zx::Status> {
    let task = inspect_runtime::publish(
        inspector,
        PublishOptions::default()
            .inspect_tree_name(UFS_INSPECT_TREE_NAME)
            .on_inspect_sink_client(client),
    )
    .ok_or(zx::Status::INTERNAL)?;
    scope.spawn_local(task);
    Ok(())
}

/// Block operation kind and logical byte count recorded in [`BlockIoStats`] on `Ok(())` completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockIoOp {
    Read(u64),
    Write(u64),
    Flush,
    Trim(u64),
}

/// Sentinel stored in [`BlockIoStats::last_doorbell_ns`] before the first submission.
const NO_DOORBELL_NS: u64 = u64::MAX;

/// Lock-free block I/O operation, byte, and inter-request gap counters exported on `root/ufs`
/// via lazy Inspect.
#[derive(Debug)]
pub struct BlockIoStats {
    pub read_ops: AtomicU64,
    pub write_ops: AtomicU64,
    pub flush_ops: AtomicU64,
    pub trim_ops: AtomicU64,
    pub read_bytes: AtomicU64,
    pub write_bytes: AtomicU64,
    pub trim_bytes: AtomicU64,
    /// Monotonic timestamp (ns) of the most recent doorbell, or [`NO_DOORBELL_NS`] before the
    /// first.
    last_doorbell_ns: AtomicU64,
    /// Counts of gaps between consecutive doorbells.
    ///
    /// Bucket 0 counts gaps under 1 us, and bucket `i` counts gaps of `2^(i - 1)` to `2^i - 1`
    /// us; the last bucket also counts longer gaps. Every doorbell wakes the link, so short gaps
    /// show requests arriving faster than the low-power timers.
    request_gap_buckets: [AtomicU64; REQUEST_GAP_BUCKETS],
}

impl Default for BlockIoStats {
    fn default() -> Self {
        Self {
            read_ops: AtomicU64::new(0),
            write_ops: AtomicU64::new(0),
            flush_ops: AtomicU64::new(0),
            trim_ops: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
            trim_bytes: AtomicU64::new(0),
            last_doorbell_ns: AtomicU64::new(NO_DOORBELL_NS),
            request_gap_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl BlockIoStats {
    pub fn record(&self, op: Option<BlockIoOp>) {
        match op {
            Some(BlockIoOp::Read(bytes)) => {
                self.read_ops.fetch_add(1, Ordering::Relaxed);
                self.read_bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            Some(BlockIoOp::Write(bytes)) => {
                self.write_ops.fetch_add(1, Ordering::Relaxed);
                self.write_bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            Some(BlockIoOp::Flush) => {
                self.flush_ops.fetch_add(1, Ordering::Relaxed);
            }
            Some(BlockIoOp::Trim(bytes)) => {
                self.trim_ops.fetch_add(1, Ordering::Relaxed);
                self.trim_bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            None => {}
        }
    }

    /// Records inter-request timing in `request_gap_buckets` when a command is submitted.
    pub fn on_request_submitted(&self, now: zx::MonotonicInstant) {
        let now_ns = u64::try_from(now.into_nanos()).unwrap_or(0);
        // ORDERING: Approximate inter-doorbell telemetry histogram; no state synchronization.
        let prev_ns = self.last_doorbell_ns.swap(now_ns, Ordering::Relaxed);
        if prev_ns != NO_DOORBELL_NS {
            let gap_us = now_ns.saturating_sub(prev_ns) / 1000;
            let bucket = (u64::BITS - gap_us.leading_zeros()) as usize;
            // ORDERING: Approximate per-bucket telemetry counter.
            self.request_gap_buckets[bucket.min(REQUEST_GAP_BUCKETS - 1)]
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_inspect(&self, node: &inspect::Node) {
        node.record_uint("block.read.count", self.read_ops.load(Ordering::Relaxed));
        node.record_uint("block.write.count", self.write_ops.load(Ordering::Relaxed));
        node.record_uint("block.flush.count", self.flush_ops.load(Ordering::Relaxed));
        node.record_uint("block.trim.count", self.trim_ops.load(Ordering::Relaxed));
        node.record_uint("block.read.kib", self.read_bytes.load(Ordering::Relaxed) / 1024);
        node.record_uint("block.write.kib", self.write_bytes.load(Ordering::Relaxed) / 1024);
        node.record_uint("block.flush.kib", 0);
        node.record_uint("block.trim.kib", self.trim_bytes.load(Ordering::Relaxed) / 1024);
        let request_gap_us = node.create_uint_exponential_histogram(
            "request_gap_us",
            inspect::ExponentialHistogramParams {
                floor: 0,
                initial_step: 1,
                step_multiplier: 2,
                buckets: REQUEST_GAP_BUCKETS - 1,
            },
        );
        for (bucket, count) in self.request_gap_buckets.iter().enumerate() {
            // ORDERING: Approximate per-bucket telemetry counter read for Inspect.
            let count = count.load(Ordering::Relaxed);
            if count > 0 {
                // Bucket 0 is `[0, 1)` us, bucket `i` (`1..27`) starts at `2^(i - 1)` us, and
                // bucket 27 (`>= 2^26` us) lands in the Inspect histogram overflow bucket.
                let gap_us = if bucket == 0 { 0 } else { 1 << (bucket - 1) };
                request_gap_us.insert_multiple(gap_us, count as usize);
            }
        }
        node.record(request_gap_us);
    }
}

/// Controller status and read-to-clear UIC error registers captured when a slot crosses its
/// deadline. One snapshot is shared across every slot that expires in the same pass.
#[derive(Debug, Clone, Copy)]
pub struct TimeoutRegisterSnapshot {
    /// Host Controller Status (`HCS`).
    pub hcs: u32,
    /// Interrupt Status (`IS`).
    pub is: u32,
    /// UTP Transfer Request List Door Bell (`UTRLDBR`).
    pub utrldbr: u32,
    /// Host UIC Error Code PHY Adapter Layer (`UECPA`).
    pub uecpa: u32,
    /// Host UIC Error Code Data Link Layer (`UECDL`).
    pub uecdl: u32,
    /// Host UIC Error Code Network Layer (`UECN`).
    pub uecn: u32,
    /// Host UIC Error Code Transport Layer (`UECT`).
    pub uect: u32,
    /// Host UIC Error Code DME (`UECDME`).
    pub uecdme: u32,
}

impl TimeoutRegisterSnapshot {
    /// Logs `msg` at `ERROR` with the expired slot and the register snapshot as structured fields.
    pub fn log_error(&self, slot: u8, scsi_opcode: u8, timeout_ms: i64, msg: &str) {
        error!(
            slot,
            scsi_opcode,
            timeout_ms,
            hcs = self.hcs,
            is = self.is,
            utrldbr = self.utrldbr,
            uecpa = self.uecpa,
            uecdl = self.uecdl,
            uecn = self.uecn,
            uect = self.uect,
            uecdme = self.uecdme;
            "{msg}"
        );
    }
}

impl<M: Mmio> TransferRequestProcessor<M> {
    /// Records lazy block I/O counters and timeout state directly on `ufs_node`.
    pub fn record_inspect(self: &Arc<Self>, ufs_node: &inspect::Node)
    where
        M: Send + Sync + 'static,
    {
        let this = Arc::downgrade(self);
        ufs_node.record_lazy_values("io_counters", move || {
            let this = this.clone();
            Box::pin(async move {
                let inspector = Inspector::default();
                if let Some(processor) = this.upgrade() {
                    let root = inspector.root();
                    processor.io_stats().record_inspect(root);
                    processor.record_admission_inspect(root);
                }
                Ok(inspector)
            })
        });
    }

    /// Records client IO admission and slot states.
    ///
    /// Helps distinguish slot exhaustion from stalling in the wrong power state.
    fn record_admission_inspect(&self, node: &inspect::Node) {
        let reqs = self.requests();
        node.record_bool("client_io_gated", !reqs.power().allows_client_io());
        node.record_uint("blocked_reservers", reqs.blocked_reservers() as u64);
        node.record_uint("blocked_on_admission", reqs.blocked_on_admission() as u64);
        node.record_uint("inflight_io", u64::from(reqs.inflight_io()));
        node.record_uint("free_mask", reqs.free_mask());
        node.record_uint("timeout_mask", reqs.timeout_mask());
    }
}

/// Device feature flags the driver last wrote or a `fuchsia.hardware.ufs` client last mutated,
/// cached for power transitions and Inspect, plus `bBootLunEn` so `UfsServer` can update its
/// Inspect property without locking `DeviceManager`.
///
/// Shared between `DeviceManager` and `UfsServer`. Client writes hold an active transaction guard,
/// and power transitions run only after in-flight client transactions drain and before IO
/// admission reopens, so `PowerWorker` and `UfsServer` never access this cache concurrently.
/// [`Self::attach_inspect`] runs once during bring-up before concurrent mutations begin.
#[derive(Debug)]
pub struct DeviceFlags {
    background_ops_enabled: AtomicBool,
    wb_enabled: AtomicBool,
    wb_flush_enabled: AtomicBool,
    wb_flush_hibernate_requested: AtomicBool,
    power_on_wp_enabled: AtomicBool,
    wb_dedicated_lun: AtomicU8,
    boot_lun_en: AtomicU32,
    inspect: OnceLock<DeviceFlagsInspect>,
}

impl Default for DeviceFlags {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct DeviceFlagsInspect {
    is_background_op_enabled: inspect::BoolProperty,
    is_write_booster_enabled: inspect::BoolProperty,
    writebooster_buffer_flush_enabled: inspect::BoolProperty,
    writebooster_buffer_flush_during_hibernate: inspect::BoolProperty,
    is_power_on_write_protect_enabled: inspect::BoolProperty,
    b_boot_lun_en: inspect::UintProperty,
}

impl DeviceFlags {
    pub fn new() -> Self {
        Self {
            background_ops_enabled: AtomicBool::new(false),
            wb_enabled: AtomicBool::new(false),
            wb_flush_enabled: AtomicBool::new(false),
            wb_flush_hibernate_requested: AtomicBool::new(false),
            power_on_wp_enabled: AtomicBool::new(false),
            wb_dedicated_lun: AtomicU8::new(NO_DEDICATED_WB_LUN),
            boot_lun_en: AtomicU32::new(0),
            inspect: OnceLock::new(),
        }
    }

    /// Creates the flag properties under `controller/background_operations`,
    /// `controller/writebooster`, and `controller/write_protect`, and `bBootLunEn` under
    /// `controller/attributes`, seeded from the current cached states.
    fn attach_inspect(
        &self,
        background_op: &inspect::Node,
        writebooster: &inspect::Node,
        attributes: &inspect::Node,
        write_protect: &inspect::Node,
    ) {
        assert!(self.inspect.get().is_none(), "DeviceFlags::attach_inspect called twice");
        let bg = self.background_ops_enabled.load(Ordering::Acquire);
        let wb = self.wb_enabled.load(Ordering::Acquire);
        let wb_flush = self.wb_flush_enabled.load(Ordering::Acquire);
        let wb_hib = self.wb_flush_hibernate_requested.load(Ordering::Acquire);
        let wp = self.power_on_wp_enabled.load(Ordering::Acquire);
        let boot_lun_en = self.boot_lun_en.load(Ordering::Acquire);
        let _ = self.inspect.set(DeviceFlagsInspect {
            is_background_op_enabled: background_op.create_bool("is_background_op_enabled", bg),
            is_write_booster_enabled: writebooster.create_bool("is_write_booster_enabled", wb),
            writebooster_buffer_flush_enabled: writebooster
                .create_bool("writebooster_buffer_flush_enabled", wb_flush),
            writebooster_buffer_flush_during_hibernate: writebooster
                .create_bool("writebooster_buffer_flush_during_hibernate", wb_hib),
            is_power_on_write_protect_enabled: write_protect
                .create_bool("is_power_on_write_protect_enabled", wp),
            b_boot_lun_en: attributes.create_uint("bBootLunEn", u64::from(boot_lun_en)),
        });
    }

    /// Records the `bBootLunEn` attribute value (`0x00`..`0x02`) and updates its Inspect property.
    pub fn set_boot_lun_en(&self, value: u32) {
        self.boot_lun_en.store(value, Ordering::Release);
        if let Some(i) = self.inspect.get() {
            i.b_boot_lun_en.set(u64::from(value));
        }
    }

    /// Records the active `LuDedicated` WriteBooster buffer LUN (`None` for shared or disabled),
    /// scoping client WriteBooster flag updates in [`Self::set_from_query`] to that LUN.
    pub fn set_wb_dedicated_lun(&self, lun: Option<u8>) {
        self.wb_dedicated_lun.store(lun.unwrap_or(NO_DEDICATED_WB_LUN), Ordering::Release);
    }

    /// Records a client flag mutation at query `index`, ignoring per-LU WriteBooster flag updates
    /// that target a different LUN than the active `LuDedicated` buffer.
    pub fn set_from_query(&self, flag: fufs::FlagType, index: u8, enabled: bool) {
        if matches!(
            flag,
            fufs::FlagType::WriteBoosterEn
                | fufs::FlagType::WriteBoosterBufferFlushEn
                | fufs::FlagType::WriteBoosterBufferFlushHibernate
        ) {
            let dedicated_lun = self.wb_dedicated_lun.load(Ordering::Acquire);
            if dedicated_lun != NO_DEDICATED_WB_LUN && index != dedicated_lun {
                return;
            }
        }
        self.set(flag, enabled);
    }

    /// Records that `flag` now reads `enabled`. Flags the driver never consults are ignored.
    pub fn set(&self, flag: fufs::FlagType, enabled: bool) {
        let inspect = self.inspect.get();
        match flag {
            fufs::FlagType::BackgroundOpsEn => {
                self.background_ops_enabled.store(enabled, Ordering::Release);
                if let Some(i) = inspect {
                    i.is_background_op_enabled.set(enabled);
                }
            }
            fufs::FlagType::WriteBoosterEn => {
                self.wb_enabled.store(enabled, Ordering::Release);
                if let Some(i) = inspect {
                    i.is_write_booster_enabled.set(enabled);
                }
            }
            fufs::FlagType::WriteBoosterBufferFlushEn => {
                self.wb_flush_enabled.store(enabled, Ordering::Release);
                if let Some(i) = inspect {
                    i.writebooster_buffer_flush_enabled.set(enabled);
                }
            }
            fufs::FlagType::WriteBoosterBufferFlushHibernate => {
                self.wb_flush_hibernate_requested.store(enabled, Ordering::Release);
                if let Some(i) = inspect {
                    i.writebooster_buffer_flush_during_hibernate.set(enabled);
                }
            }
            fufs::FlagType::PowerOnWpEn => {
                self.power_on_wp_enabled.store(enabled, Ordering::Release);
                if let Some(i) = inspect {
                    i.is_power_on_write_protect_enabled.set(enabled);
                }
            }
            _ => {}
        }
    }

    /// Clears volatile device flags reset by `PowerDown` while preserving `wb_enabled` and
    /// `wb_flush_hibernate_requested` so `restore_config` re-applies the active configuration
    /// without overwriting a prior client `ClearFlag(WriteBoosterBufferFlushHibernate)` request.
    pub fn clear_volatile_for_power_down(&self) {
        // Leave `power_on_wp_enabled` untouched: it records what to restore, not what the device
        // currently holds.
        self.background_ops_enabled.store(false, Ordering::Release);
        self.wb_flush_enabled.store(false, Ordering::Release);
        if let Some(i) = self.inspect.get() {
            i.is_background_op_enabled.set(false);
            i.writebooster_buffer_flush_enabled.set(false);
            i.writebooster_buffer_flush_during_hibernate.set(false);
        }
    }

    #[cfg(test)]
    pub fn background_ops_enabled(&self) -> bool {
        self.background_ops_enabled.load(Ordering::Acquire)
    }

    pub fn wb_enabled(&self) -> bool {
        self.wb_enabled.load(Ordering::Acquire)
    }

    pub fn power_on_wp_enabled(&self) -> bool {
        self.power_on_wp_enabled.load(Ordering::Acquire)
    }

    pub fn wb_flush_enabled(&self) -> bool {
        self.wb_flush_enabled.load(Ordering::Acquire)
    }

    pub fn wb_flush_hibernate_requested(&self) -> bool {
        self.wb_flush_hibernate_requested.load(Ordering::Acquire)
    }
}

/// Creates an exponential microsecond latency histogram (`floor = 1 us`, `step_multiplier = 2`)
/// on `node`.
pub fn make_exp_hist(
    node: &inspect::Node,
    name: &str,
    buckets: usize,
) -> inspect::UintExponentialHistogramProperty {
    node.create_uint_exponential_histogram(
        name,
        inspect::ExponentialHistogramParams {
            floor: 1,
            initial_step: 1,
            step_multiplier: 2,
            buckets,
        },
    )
}

pub fn record_elapsed_us(
    hist: &inspect::UintExponentialHistogramProperty,
    start: zx::MonotonicInstant,
) {
    let elapsed_us = (zx::MonotonicInstant::get() - start).into_micros().max(0) as u64;
    hist.insert(elapsed_us);
}

/// Inspect nodes and properties owned by [`DeviceManager`] under `root/ufs`.
#[derive(Debug, Default)]
pub struct DeviceInspect {
    _controller_node: inspect::Node,
    _attributes_node: inspect::Node,
    _unipro_node: inspect::Node,
    _background_op_node: inspect::Node,
    _writebooster_node: inspect::Node,
    _write_protect_node: inspect::Node,
    pub power_suspended: inspect::BoolProperty,
    pub power_step: inspect::StringProperty,
    pub suspend_failures: inspect::UintProperty,
    pub link_vote_error_count: inspect::UintProperty,
    pub suspend_latency_us: inspect::UintExponentialHistogramProperty,
    pub wake_latency_us: inspect::UintExponentialHistogramProperty,
    _max_transfer_bytes: inspect::UintProperty,
    pub logical_unit_count: inspect::UintProperty,
    pub reference_clock: inspect::StringProperty,
    pub power_condition: inspect::UintProperty,
    pub link_state: inspect::UintProperty,
    pub b_current_power_mode: inspect::UintProperty,
    pub b_active_icc_level: inspect::UintProperty,
    pub remote_version: inspect::UintProperty,
    pub local_version: inspect::UintProperty,
    pub host_t_activate: inspect::UintProperty,
    pub device_t_activate: inspect::UintProperty,
    pub host_granularity: inspect::UintProperty,
    pub device_granularity: inspect::UintProperty,
    pub pa_active_tx_data_lanes: inspect::UintProperty,
    pub pa_active_rx_data_lanes: inspect::UintProperty,
    pub pa_max_rx_hs_gear: inspect::UintProperty,
    pub pa_tx_gear: inspect::UintProperty,
    pub pa_rx_gear: inspect::UintProperty,
    pub tx_termination: inspect::BoolProperty,
    pub rx_termination: inspect::BoolProperty,
    pub pa_hs_series: inspect::UintProperty,
    pub power_mode: inspect::UintProperty,
    pub write_booster_buffer_type: inspect::UintProperty,
    pub user_space_configuration_option: inspect::UintProperty,
    pub write_booster_dedicated_lu: inspect::UintProperty,
    pub write_booster_buffer_size_in_bytes: inspect::UintProperty,
}

impl DeviceInspect {
    pub fn new(ufs_node: &inspect::Node, flags: &DeviceFlags) -> Self {
        let controller_node = ufs_node.create_child("controller");
        let attributes_node = controller_node.create_child("attributes");
        let unipro_node = controller_node.create_child("unipro");
        let background_op_node = controller_node.create_child("background_operations");
        let writebooster_node = controller_node.create_child("writebooster");
        let write_protect_node = controller_node.create_child("write_protect");
        flags.attach_inspect(
            &background_op_node,
            &writebooster_node,
            &attributes_node,
            &write_protect_node,
        );
        Self {
            power_suspended: ufs_node.create_bool("power_suspended", false),
            power_step: ufs_node.create_string("power_step", POWER_STEP_IDLE),
            suspend_failures: ufs_node.create_uint("suspend_failures", 0),
            link_vote_error_count: ufs_node.create_uint("link_vote_error_count", 0),
            suspend_latency_us: make_exp_hist(ufs_node, "suspend_latency_us", 20),
            wake_latency_us: make_exp_hist(ufs_node, "wake_latency_us", 20),
            _max_transfer_bytes: controller_node
                .create_uint("max_transfer_bytes", MAX_TRANSFER_BYTES),
            logical_unit_count: controller_node.create_uint("logical_unit_count", 0),
            reference_clock: controller_node.create_string("reference_clock", ""),
            power_condition: controller_node
                .create_uint("PowerCondition", StartStopPowerCondition::Active as u64),
            link_state: controller_node.create_uint("LinkState", LinkState::Active as u64),
            b_current_power_mode: attributes_node
                .create_uint("bCurrentPowerMode", UfsPowerMode::Active as u64),
            b_active_icc_level: attributes_node
                .create_uint("bActiveICCLevel", u64::from(HIGHEST_ACTIVE_ICC_LEVEL)),
            remote_version: unipro_node.create_uint("remote_version", 0),
            local_version: unipro_node.create_uint("local_version", 0),
            host_t_activate: unipro_node.create_uint("host_t_activate", 0),
            device_t_activate: unipro_node.create_uint("device_t_activate", 0),
            host_granularity: unipro_node.create_uint("host_granularity", 0),
            device_granularity: unipro_node.create_uint("device_granularity", 0),
            pa_active_tx_data_lanes: unipro_node.create_uint("PA_ActiveTxDataLanes", 0),
            pa_active_rx_data_lanes: unipro_node.create_uint("PA_ActiveRxDataLanes", 0),
            pa_max_rx_hs_gear: unipro_node.create_uint("PA_MaxRxHSGear", 0),
            pa_tx_gear: unipro_node.create_uint("PA_TxGear", 0),
            pa_rx_gear: unipro_node.create_uint("PA_RxGear", 0),
            tx_termination: unipro_node.create_bool("tx_termination", false),
            rx_termination: unipro_node.create_bool("rx_termination", false),
            pa_hs_series: unipro_node.create_uint("PA_HSSeries", 0),
            power_mode: unipro_node.create_uint("power_mode", 0),
            write_booster_buffer_type: writebooster_node
                .create_uint("write_booster_buffer_type", 0),
            user_space_configuration_option: writebooster_node
                .create_uint("user_space_configuration_option", 0),
            write_booster_dedicated_lu: writebooster_node
                .create_uint("write_booster_dedicated_lu", 0),
            write_booster_buffer_size_in_bytes: writebooster_node
                .create_uint("write_booster_buffer_size_in_bytes", 0),
            _controller_node: controller_node,
            _attributes_node: attributes_node,
            _unipro_node: unipro_node,
            _background_op_node: background_op_node,
            _writebooster_node: writebooster_node,
            _write_protect_node: write_protect_node,
        }
    }
}

impl DeviceManager {
    /// Attaches the `controller` Inspect subtree and properties under `ufs_node`.
    pub fn init_inspect(&mut self, ufs_node: &inspect::Node) {
        self.set_inspect(DeviceInspect::new(ufs_node, self.flags()));
    }

    /// Records the number of bound logical units in `controller/logical_unit_count`.
    pub fn record_logical_unit_count(&self, count: usize) {
        self.inspect().logical_unit_count.set(count as u64);
    }

    pub fn sync_power_inspect(&self, state: PowerState) {
        let (mode, cond, link) = state.device_and_link_states();
        let inspect = self.inspect();
        inspect.power_condition.set(cond as u64);
        inspect.link_state.set(link as u64);
        inspect.b_current_power_mode.set(mode as u64);
        inspect.power_suspended.set(state != PowerState::Active);
    }
}

#[cfg(test)]
mod tests {
    use diagnostics_assertions::{AnyProperty, assert_data_tree};
    use fidl::endpoints::create_endpoints;
    use fuchsia_inspect as inspect;
    use futures::StreamExt;
    use mmio::{Mmio, Register};

    use super::*;
    use crate::constants::{FAST_PWR_MODE, HS_SERIES_B};
    use crate::quirks::HostQuirks;
    use crate::registers::{CapabilityReg, PowerModeStatus, VersionReg};
    use crate::testing::fake_ufs::{CAP_AUTO_HIBERNATE_SUPPORT, hcs_power_mode_status};
    use crate::testing::{
        POWER_MODE_CHANGE_UIC_COMMANDS_G4, create_test_processor, fake_uic_device,
        link_capabilities, ufs_histogram_samples,
    };

    #[fuchsia::test]
    async fn test_publish_ufs_inspect_sets_ufs_tree_name() {
        let inspector = inspect::Inspector::default();
        inspector.root().record_uint("probe", 42);
        let scope = fasync::Scope::new();
        let (client, server) = create_endpoints::<finspect::InspectSinkMarker>();
        publish_ufs_inspect_with_client(&inspector, client, scope.to_handle()).unwrap();

        let mut stream = server.into_stream();
        match stream.next().await {
            Some(Ok(finspect::InspectSinkRequest::Publish { payload, .. })) => {
                assert_eq!(payload.name.as_deref(), Some(UFS_INSPECT_TREE_NAME));
                assert!(payload.tree.is_some());
            }
            other => panic!("expected InspectSink::Publish request, got {other:?}"),
        }
        scope.cancel().await;
    }

    #[fuchsia::test]
    async fn test_device_flags_sync_before_and_after_attach() {
        let flags = DeviceFlags::new();
        flags.set(fufs::FlagType::BackgroundOpsEn, true);
        flags.set(fufs::FlagType::WriteBoosterEn, true);
        flags.set(fufs::FlagType::WriteBoosterBufferFlushEn, true);
        flags.set(fufs::FlagType::WriteBoosterBufferFlushHibernate, true);

        let inspector = inspect::Inspector::default();
        let ufs = inspector.root().create_child("ufs");
        let _dev = DeviceInspect::new(&ufs, &flags);
        inspector.root().record(ufs);
        assert_data_tree!(inspector, root: contains {
            ufs: contains {
                controller: contains {
                    background_operations: {
                        is_background_op_enabled: true,
                    },
                    writebooster: contains {
                        is_write_booster_enabled: true,
                        writebooster_buffer_flush_enabled: true,
                        writebooster_buffer_flush_during_hibernate: true,
                    },
                },
            },
        });

        flags.set(fufs::FlagType::WriteBoosterBufferFlushHibernate, false);
        assert_data_tree!(inspector, root: contains {
            ufs: contains {
                controller: contains {
                    background_operations: {
                        is_background_op_enabled: true,
                    },
                    writebooster: contains {
                        is_write_booster_enabled: true,
                        writebooster_buffer_flush_enabled: true,
                        writebooster_buffer_flush_during_hibernate: false,
                    },
                },
            },
        });
    }

    #[fuchsia::test]
    async fn test_request_gap_histogram_records_gaps_between_doorbells() {
        use fuchsia_inspect::reader::{ArrayContent, Property};

        let (_mmio_vmo, processor) = create_test_processor();
        let inspector = inspect::Inspector::default();
        let ufs_node = inspector.root().create_child("ufs");
        processor.record_inspect(&ufs_node);
        // The first doorbell has no earlier doorbell; the next three record gaps of 0 us, 3 us,
        // and 2^26 us (overflow).
        let start = zx::MonotonicInstant::get();
        for offset_us in [0, 0, 3, 3 + (1 << 26)] {
            let now = start + zx::MonotonicDuration::from_micros(offset_us);
            processor.io_stats().on_request_submitted(now);
        }
        inspector.root().record(ufs_node);
        let hierarchy = inspect::reader::read(&inspector).await.unwrap();
        assert_eq!(ufs_histogram_samples(&hierarchy, "request_gap_us"), 3);
        match hierarchy.get_child("ufs").and_then(|n| n.get_property("request_gap_us")) {
            Some(Property::UintArray(_, ArrayContent::ExponentialHistogram(hist))) => {
                assert_eq!(hist.floor, 0);
                assert_eq!(hist.initial_step, 1);
                assert_eq!(hist.step_multiplier, 2);
                assert_eq!(hist.size, REQUEST_GAP_BUCKETS + 1);
                let non_zero: Vec<(usize, u64)> = match &hist.indexes {
                    Some(idxs) => idxs.iter().copied().zip(hist.counts.iter().copied()).collect(),
                    None => hist
                        .counts
                        .iter()
                        .copied()
                        .enumerate()
                        .filter(|&(_, count)| count > 0)
                        .collect(),
                };
                // Index 0 is underflow (< 0), index 1 is [0, 1), index 3 is [2, 4), and index 28
                // (`REQUEST_GAP_BUCKETS`) is overflow ([2^26, u64::MAX)).
                assert_eq!(non_zero, vec![(1, 1), (3, 1), (REQUEST_GAP_BUCKETS, 1)]);
            }
            other => panic!("unexpected request_gap_us property: {other:?}"),
        }

        // Verify a 0 ns timestamp is distinct from `NO_DOORBELL_NS` (`u64::MAX`).
        let zero_stats = BlockIoStats::default();
        zero_stats.on_request_submitted(zx::MonotonicInstant::from_nanos(0));
        zero_stats.on_request_submitted(zx::MonotonicInstant::from_nanos(0));
        // ORDERING: single-threaded unit test.
        assert_eq!(zero_stats.request_gap_buckets[0].load(Ordering::Relaxed), 1);
    }

    #[fuchsia::test]
    async fn test_inspect_hierarchy_recording() {
        let (mmio_vmo, processor) = create_test_processor();
        processor.controller().with_mmio(|mmio| {
            mmio.store32(VersionReg::OFFSET, 4 << 8);
            mmio.store32(
                CapabilityReg::OFFSET,
                CAP_AUTO_HIBERNATE_SUPPORT | (1 << 24) | (7 << 16) | (15 << 8) | 31,
            );
        });
        let inspector = inspect::Inspector::default();
        let ufs_node = inspector.root().create_child("ufs");
        processor.controller().record_inspect(&ufs_node);
        processor.record_inspect(&ufs_node);
        processor.io_stats().record(Some(BlockIoOp::Read(8192)));
        processor.io_stats().record(Some(BlockIoOp::Write(4096)));
        processor.io_stats().record(Some(BlockIoOp::Flush));
        processor.io_stats().record(Some(BlockIoOp::Trim(16384)));

        let mut device_manager = DeviceManager::new(HostQuirks::default());
        device_manager.init_inspect(&ufs_node);
        device_manager.record_logical_unit_count(3);

        let hcs = hcs_power_mode_status(PowerModeStatus::PowerLocal);
        let hardware = fake_uic_device(
            &mmio_vmo,
            hcs,
            POWER_MODE_CHANGE_UIC_COMMANDS_G4,
            link_capabilities(4, 4),
        );
        assert_eq!(device_manager.init_uic_power_mode(&processor, None).await, Ok(()));
        hardware.join().unwrap();
        inspector.root().record(ufs_node);

        assert_data_tree!(inspector, root: {
            ufs: {
                power_suspended: false,
                power_step: "idle",
                suspend_failures: 0u64,
                link_vote_error_count: 0u64,
                suspend_latency_us: AnyProperty,
                wake_latency_us: AnyProperty,
                request_gap_us: AnyProperty,
                version: {
                    major_version_number: 4u64,
                    minor_version_number: 0u64,
                    version_suffix: 0u64,
                },
                capabilities: {
                    crypto_support: false,
                    uic_dme_test_mode_command_supported: false,
                    out_of_order_data_delivery_supported: false,
                    "64_bit_addressing_supported": true,
                    auto_hibernation_support: true,
                    number_of_utp_task_management_request_slots: 7u64,
                    number_of_outstanding_rtt_requests_supported: 15u64,
                    number_of_utp_transfer_request_slots: 31u64,
                },
                uic_error_count: 0u64,
                fatal_error_count: 0u64,
                auto_hibern8_error_count: 0u64,
                missed_interrupt_count: 0u64,
                "block.read.count": 1u64,
                "block.write.count": 1u64,
                "block.flush.count": 1u64,
                "block.trim.count": 1u64,
                "block.read.kib": 8u64,
                "block.write.kib": 4u64,
                "block.flush.kib": 0u64,
                "block.trim.kib": 16u64,
                client_io_gated: false,
                blocked_reservers: 0u64,
                blocked_on_admission: 0u64,
                inflight_io: 0u64,
                free_mask: AnyProperty,
                timeout_mask: 0u64,
                controller: {
                    max_transfer_bytes: MAX_TRANSFER_BYTES,
                    logical_unit_count: 3u64,
                    reference_clock: "",
                    PowerCondition: 1u64,
                    LinkState: 1u64,
                    attributes: {
                        bBootLunEn: 0u64,
                        bCurrentPowerMode: UfsPowerMode::Active as u64,
                        bActiveICCLevel: u64::from(HIGHEST_ACTIVE_ICC_LEVEL),
                    },
                    unipro: {
                        remote_version: 0u64,
                        local_version: 0u64,
                        host_t_activate: 0u64,
                        device_t_activate: 0u64,
                        host_granularity: 0u64,
                        device_granularity: 0u64,
                        PA_ActiveTxDataLanes: 2u64,
                        PA_ActiveRxDataLanes: 2u64,
                        PA_MaxRxHSGear: 4u64,
                        PA_TxGear: 4u64,
                        PA_RxGear: 4u64,
                        tx_termination: true,
                        rx_termination: true,
                        PA_HSSeries: u64::from(HS_SERIES_B),
                        power_mode: u64::from(FAST_PWR_MODE),
                    },
                    background_operations: {
                        is_background_op_enabled: false,
                    },
                    writebooster: {
                        is_write_booster_enabled: false,
                        writebooster_buffer_flush_during_hibernate: false,
                        writebooster_buffer_flush_enabled: false,
                        write_booster_buffer_type: 0u64,
                        user_space_configuration_option: 0u64,
                        write_booster_dedicated_lu: 0u64,
                        write_booster_buffer_size_in_bytes: 0u64,
                    },
                    write_protect: {
                        is_power_on_write_protect_enabled: false,
                    },
                },
            }
        });
    }
}
