// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Best-effort system CPU, memory, GPU, power, and Scenic frame telemetry collection.
//!
//! [`DiagnosticsCollector`] aggregates telemetry concurrently from five metric collectors:
//! - [`CpuCollector`] ([`CpuMetrics`]): average load, per-core load, and CPU power draw via
//!   `fuchsia.kernel.Stats::GetCpuStats`.
//! - [`MemoryCollector`] ([`MemoryMetrics`]): physical memory, VMO allocation, and ZRAM
//!   compression via `fuchsia.kernel.Stats`.
//! - [`GpuCollector`] ([`GpuMetrics`]): active GPU utilization via `fuchsia.gpu.magma.Device`
//!   (`MagmaQueryTotalTime`).
//! - [`PowerCollector`] ([`PowerMetrics`]): battery charge, temperature, fuel-gauge power draw,
//!   and Zircon active/boot uptime with suspend residency via
//!   `fuchsia.power.battery.BatteryManager` (or `fuchsia.hardware.power.battery.Service` fallback)
//!   and Zircon clocks.
//! - [`ScenicCollector`] ([`ScenicMetrics`]): Scenic `FrameScheduler` frame counter and FPS via
//!   `fuchsia.diagnostics.ArchiveAccessor`.
//!
//! Every source is optional: missing capabilities or transient FIDL errors yield `None` on the
//! corresponding metric fields rather than failing collection.
//!
//! # Examples
//!
//! ```no_run
//! use debug_telemetry::{CachePolicy, DiagnosticsCollector};
//!
//! async fn sample_once() {
//!     let mut collector = DiagnosticsCollector::from_namespace();
//!     collector.prime_baseline().await;
//!     let snapshot = collector.snapshot(CachePolicy::ForceRefresh).await;
//!     let _ = snapshot.cpu.avg_load_pct;
//! }
//! ```

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod cpu;
pub mod gpu;
pub mod memory;
pub mod power;
pub mod scenic;

pub use cpu::{CpuCollector, CpuMetrics};
pub use gpu::{GpuCollector, GpuMetrics};
pub use memory::{MemoryCollector, MemoryMetrics};
pub use power::{ChargeSource, ChargeStatus, PowerCollector, PowerMetrics};
pub use scenic::{DEFAULT_SCENIC_FRAME_NUMBER_SELECTOR, ScenicCollector, ScenicMetrics};

use fidl_fuchsia_kernel as fkernel;
use fuchsia_component::client::connect_to_protocol;
use std::future::Future;

/// Controls whether [`DiagnosticsCollector::snapshot`] reuses an existing cached sample or
/// performs a fresh collection pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    /// Returns the most recently collected [`DiagnosticSnapshot`] if one exists, or collects a
    /// fresh snapshot when the cache is cold.
    UseCached,
    /// Collects a fresh [`DiagnosticSnapshot`] and updates the cache.
    ForceRefresh,
}

/// Point-in-time snapshot of system CPU, GPU, memory, power/uptime, and Scenic frame metrics.
#[derive(Debug, PartialEq, Default)]
#[non_exhaustive]
pub struct DiagnosticSnapshot {
    /// Monotonically increasing counter (`>= 1` once collected) identifying each fresh sample.
    pub generation: u64,
    /// Average and per-core CPU load and package power consumption.
    pub cpu: CpuMetrics,
    /// Active GPU utilization percentage.
    pub gpu: GpuMetrics,
    /// Physical memory, VMO allocation, and ZRAM compression statistics.
    pub memory: MemoryMetrics,
    /// Battery state, fuel-gauge power flow, active/boot uptime, and suspend residency.
    pub power: PowerMetrics,
    /// Scenic `FrameScheduler` cumulative frame counter and recent frame rate.
    pub scenic: ScenicMetrics,
}

impl Clone for DiagnosticSnapshot {
    fn clone(&self) -> Self {
        Self {
            generation: self.generation,
            cpu: self.cpu.clone(),
            gpu: self.gpu,
            memory: self.memory,
            power: self.power,
            scenic: self.scenic,
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.generation = source.generation;
        self.cpu.clone_from(&source.cpu);
        self.gpu = source.gpu;
        self.memory = source.memory;
        self.power = source.power;
        self.scenic = source.scenic;
    }
}

/// Builder for selectively enabling [`DiagnosticsCollector`] metric sources from the incoming
/// component namespace.
///
/// Disabled collectors are initialized via `disconnected()`, avoiding unnecessary namespace
/// connections or background polling for capabilities a component does not use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiagnosticsCollectorBuilder {
    cpu: bool,
    gpu: bool,
    memory: bool,
    power: bool,
    scenic: bool,
}

impl DiagnosticsCollectorBuilder {
    /// Creates a builder with all metric collectors initially disabled.
    pub const fn new() -> Self {
        Self { cpu: false, gpu: false, memory: false, power: false, scenic: false }
    }

    /// Enables [`CpuCollector`] (`fuchsia.kernel.Stats`).
    pub const fn with_cpu(mut self) -> Self {
        self.cpu = true;
        self
    }

    /// Enables [`GpuCollector`] (`fuchsia.gpu.magma.{Service,TrustedService}` / `/dev/class/gpu`).
    pub const fn with_gpu(mut self) -> Self {
        self.gpu = true;
        self
    }

    /// Enables [`MemoryCollector`] (`fuchsia.kernel.Stats`).
    pub const fn with_memory(mut self) -> Self {
        self.memory = true;
        self
    }

    /// Enables [`PowerCollector`] (`fuchsia.power.battery.BatteryManager` /
    /// `fuchsia.hardware.power.battery.Service` + Zircon clocks).
    pub const fn with_power(mut self) -> Self {
        self.power = true;
        self
    }

    /// Enables [`ScenicCollector`] (`fuchsia.diagnostics.ArchiveAccessor`).
    pub const fn with_scenic(mut self) -> Self {
        self.scenic = true;
        self
    }

    /// Connects only the enabled metric collectors from the incoming component namespace, sharing
    /// a single `fuchsia.kernel.Stats` connection when both CPU and memory are enabled.
    pub fn build_from_namespace(self) -> DiagnosticsCollector {
        self.build_with_connect(
            || connect_to_protocol::<fkernel::StatsMarker>().ok(),
            GpuCollector::from_namespace,
            PowerCollector::from_namespace,
            ScenicCollector::from_namespace,
        )
    }

    fn build_with_connect(
        self,
        connect_kernel: impl FnOnce() -> Option<fkernel::StatsProxy>,
        connect_gpu: impl FnOnce() -> GpuCollector,
        connect_power: impl FnOnce() -> PowerCollector,
        connect_scenic: impl FnOnce() -> ScenicCollector,
    ) -> DiagnosticsCollector {
        let kernel_stats = if self.cpu || self.memory { connect_kernel() } else { None };
        let cpu = if self.cpu {
            CpuCollector::with_proxy(kernel_stats.clone())
        } else {
            CpuCollector::disconnected()
        };
        let gpu = if self.gpu { connect_gpu() } else { GpuCollector::disconnected() };
        let memory = if self.memory {
            MemoryCollector::with_proxy(kernel_stats)
        } else {
            MemoryCollector::disconnected()
        };
        let power = if self.power { connect_power() } else { PowerCollector::disconnected() };
        let scenic = if self.scenic { connect_scenic() } else { ScenicCollector::disconnected() };
        let mut collector = DiagnosticsCollector::with_collectors(cpu, gpu, memory, power, scenic);
        collector.shares_kernel_stats = self.cpu && self.memory;
        collector
    }
}

/// Collects real-time telemetry concurrently across [`CpuCollector`], [`GpuCollector`],
/// [`MemoryCollector`], [`PowerCollector`], and [`ScenicCollector`], reusing internal snapshot
/// storage across refreshes.
#[derive(Debug)]
pub struct DiagnosticsCollector {
    cpu: CpuCollector,
    gpu: GpuCollector,
    memory: MemoryCollector,
    power: PowerCollector,
    scenic: ScenicCollector,
    snapshot: DiagnosticSnapshot,
    shares_kernel_stats: bool,
}

impl DiagnosticsCollector {
    /// Returns a [`DiagnosticsCollectorBuilder`] with all collectors initially disabled so callers
    /// can opt into specific metric sources.
    pub const fn builder() -> DiagnosticsCollectorBuilder {
        DiagnosticsCollectorBuilder::new()
    }

    /// Connects to available telemetry protocols in the incoming namespace, sharing a single
    /// `fuchsia.kernel.Stats` connection between [`CpuCollector`] and [`MemoryCollector`].
    pub fn from_namespace() -> Self {
        Self::builder()
            .with_cpu()
            .with_gpu()
            .with_memory()
            .with_power()
            .with_scenic()
            .build_from_namespace()
    }

    /// Creates an inert `DiagnosticsCollector` with all protocol connections disabled.
    pub fn disconnected() -> Self {
        Self::with_collectors(
            CpuCollector::disconnected(),
            GpuCollector::disconnected(),
            MemoryCollector::disconnected(),
            PowerCollector::disconnected(),
            ScenicCollector::disconnected(),
        )
    }

    /// Creates a `DiagnosticsCollector` from explicit metric collectors (useful for testing or
    /// custom capability routing).
    pub fn with_collectors(
        cpu: CpuCollector,
        gpu: GpuCollector,
        memory: MemoryCollector,
        power: PowerCollector,
        scenic: ScenicCollector,
    ) -> Self {
        Self {
            cpu,
            gpu,
            memory,
            power,
            scenic,
            snapshot: DiagnosticSnapshot::default(),
            shares_kernel_stats: false,
        }
    }

    fn sync_shared_kernel_stats(&mut self) {
        if self.shares_kernel_stats && (!self.cpu.is_connected() || !self.memory.is_connected()) {
            self.cpu.disconnect();
            self.memory.disconnect();
        }
    }

    /// Returns the most recently cached [`DiagnosticSnapshot`], if any.
    pub fn cached_snapshot(&self) -> Option<&DiagnosticSnapshot> {
        (self.snapshot.generation != 0).then_some(&self.snapshot)
    }

    /// Captures baseline CPU idle-time, GPU total-time, and Scenic frame-number samples so the
    /// next [`collect`](Self::collect) call can immediately compute window deltas.
    pub async fn prime_baseline(&mut self) {
        futures::join!(
            self.cpu.prime_baseline(),
            self.gpu.prime_baseline(),
            self.scenic.prime_baseline(),
        );
        self.sync_shared_kernel_stats();
    }

    /// Captures baseline CPU, GPU, and Scenic samples using `monotonic` as the CPU/Scenic
    /// reference timestamp.
    pub async fn prime_baseline_at(&mut self, monotonic: zx::MonotonicInstant) {
        futures::join!(
            self.cpu.prime_baseline_at(monotonic),
            self.gpu.prime_baseline(),
            self.scenic.prime_baseline_at(monotonic),
        );
        self.sync_shared_kernel_stats();
    }

    /// Returns a [`DiagnosticSnapshot`] according to `policy`, reusing the cached snapshot when
    /// `policy` is [`CachePolicy::UseCached`] and a sample has already been collected.
    pub fn snapshot(&mut self, policy: CachePolicy) -> impl Future<Output = &DiagnosticSnapshot> {
        if policy == CachePolicy::UseCached && self.snapshot.generation != 0 {
            futures::future::Either::Left(futures::future::ready(&self.snapshot))
        } else {
            futures::future::Either::Right(self.collect())
        }
    }

    /// Collects a fresh [`DiagnosticSnapshot`] using current Zircon timestamps, updating the
    /// cached snapshot and incrementing its `generation`.
    pub async fn collect(&mut self) -> &DiagnosticSnapshot {
        let (_, gpu, memory, power, scenic) = futures::join!(
            self.cpu.collect_into(&mut self.snapshot.cpu),
            self.gpu.collect(),
            self.memory.collect(),
            self.power.collect(),
            self.scenic.collect(),
        );
        self.sync_shared_kernel_stats();
        self.finish_snapshot(gpu, memory, power, scenic)
    }

    /// Collects a fresh [`DiagnosticSnapshot`] stamped at `monotonic` and `boot`, updating the
    /// cached snapshot and incrementing its `generation`.
    pub async fn collect_at(
        &mut self,
        monotonic: zx::MonotonicInstant,
        boot: zx::BootInstant,
    ) -> &DiagnosticSnapshot {
        let (_, gpu, memory, power, scenic) = futures::join!(
            self.cpu.collect_at_into(monotonic, &mut self.snapshot.cpu),
            self.gpu.collect(),
            self.memory.collect(),
            self.power.collect_at(monotonic, boot),
            self.scenic.collect_at(monotonic),
        );
        self.sync_shared_kernel_stats();
        self.finish_snapshot(gpu, memory, power, scenic)
    }

    fn finish_snapshot(
        &mut self,
        gpu: GpuMetrics,
        memory: MemoryMetrics,
        power: PowerMetrics,
        scenic: ScenicMetrics,
    ) -> &DiagnosticSnapshot {
        self.snapshot.generation = self.snapshot.generation.wrapping_add(1).max(1);
        self.snapshot.gpu = gpu;
        self.snapshot.memory = memory;
        self.snapshot.power = power;
        self.snapshot.scenic = scenic;
        &self.snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_hierarchy::{DiagnosticsHierarchy, Property};
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_diagnostics as fdiagnostics;
    use fidl_fuchsia_gpu_magma as fgpu;
    use fidl_fuchsia_hardware_power_battery as fhwbattery;
    use fidl_fuchsia_kernel as fkernel;
    use fidl_fuchsia_power_battery as fbattery;
    use fuchsia_async as fasync;
    use futures::StreamExt;

    const MB: u64 = 1024 * 1024;

    fn mono(ns: i64) -> zx::MonotonicInstant {
        zx::MonotonicInstant::from_nanos(ns)
    }

    fn boot(ns: i64) -> zx::BootInstant {
        zx::BootInstant::from_nanos(ns)
    }

    fn make_magma_time_vmo(gpu_time_ns: u64, monotonic_time_ns: u64) -> zx::Vmo {
        let vmo = zx::Vmo::create(16).expect("create magma time VMO");
        let mut bytes = [0_u8; 16];
        bytes[0..8].copy_from_slice(&gpu_time_ns.to_le_bytes());
        bytes[8..16].copy_from_slice(&monotonic_time_ns.to_le_bytes());
        vmo.write(&bytes, 0).expect("write magma time VMO");
        vmo
    }

    #[fuchsia::test]
    async fn test_cpu_and_memory_collectors_share_kernel_stats_proxy() {
        let scope = fasync::Scope::new();
        let (proxy, mut stream) = create_proxy_and_stream::<fkernel::StatsMarker>();

        scope.spawn(async move {
            let mut cpu_call_index = 0_u32;
            while let Some(Ok(request)) = stream.next().await {
                match request {
                    fkernel::StatsRequest::GetCpuStats { responder } => {
                        cpu_call_index += 1;
                        let stats = if cpu_call_index == 1 {
                            fkernel::CpuStats {
                                actual_num_cpus: 2,
                                per_cpu_stats: Some(vec![
                                    fkernel::PerCpuStats {
                                        flags: Some(1),
                                        idle_time: Some(1_000_000_000),
                                        active_energy_consumption_nj: Some(2_000_000_000),
                                        idle_energy_consumption_nj: Some(500_000_000),
                                        ..Default::default()
                                    },
                                    fkernel::PerCpuStats {
                                        flags: Some(1),
                                        idle_time: Some(2_000_000_000),
                                        active_energy_consumption_nj: Some(1_000_000_000),
                                        idle_energy_consumption_nj: Some(500_000_000),
                                        ..Default::default()
                                    },
                                ]),
                            }
                        } else {
                            fkernel::CpuStats {
                                actual_num_cpus: 2,
                                per_cpu_stats: Some(vec![
                                    fkernel::PerCpuStats {
                                        flags: Some(1),
                                        idle_time: Some(1_750_000_000),
                                        active_energy_consumption_nj: Some(2_300_000_000),
                                        idle_energy_consumption_nj: Some(600_000_000),
                                        ..Default::default()
                                    },
                                    fkernel::PerCpuStats {
                                        flags: Some(1),
                                        idle_time: Some(2_250_000_000),
                                        active_energy_consumption_nj: Some(1_500_000_000),
                                        idle_energy_consumption_nj: Some(600_000_000),
                                        ..Default::default()
                                    },
                                ]),
                            }
                        };
                        let _ = responder.send(&stats);
                    }
                    fkernel::StatsRequest::GetMemoryStats { responder } => {
                        let mem = fkernel::MemoryStats {
                            total_bytes: Some(4096 * MB),
                            free_bytes: Some(1296 * MB),
                            vmo_bytes: Some(2100 * MB),
                            ..Default::default()
                        };
                        let _ = responder.send(&mem);
                    }
                    fkernel::StatsRequest::GetMemoryStatsCompression { responder } => {
                        let comp = fkernel::MemoryStatsCompression {
                            compressed_storage_bytes: Some(128 * MB),
                            uncompressed_storage_bytes: Some(384 * MB),
                            ..Default::default()
                        };
                        let _ = responder.send(&comp);
                    }
                    _ => {}
                }
            }
        });

        let mut cpu_collector = CpuCollector::with_proxy(Some(proxy.clone()));
        let mut memory_collector = MemoryCollector::with_proxy(Some(proxy));

        cpu_collector.prime_baseline_at(mono(10_000_000_000)).await;
        let (cpu, memory) = futures::join!(
            cpu_collector.collect_at(mono(11_000_000_000)),
            memory_collector.collect(),
        );

        assert_eq!(cpu.per_core_load_pct, vec![25, 75]);
        assert!((cpu.avg_load_pct.expect("avg cpu load") - 50.0).abs() < 1e-4);
        assert_eq!(cpu.power_mw, Some(1000));
        assert_eq!(memory.total_mb, Some(4096));
        assert_eq!(memory.free_mb, Some(1296));
        assert_eq!(memory.used_mb, Some(2800));
        assert_eq!(memory.vmo_mb, Some(2100));
        assert_eq!(memory.zram_compressed_mb, Some(128));
        assert_eq!(memory.zram_uncompressed_mb, Some(384));
    }

    #[fuchsia::test]
    async fn test_cpu_collector_offline_cores_zero_delta_and_energy_gating() {
        let scope = fasync::Scope::new();
        let (proxy, mut stream) = create_proxy_and_stream::<fkernel::StatsMarker>();

        scope.spawn(async move {
            let samples = [
                vec![
                    fkernel::PerCpuStats {
                        flags: Some(1),
                        idle_time: Some(1_000_000_000),
                        active_energy_consumption_nj: Some(0),
                        idle_energy_consumption_nj: Some(0),
                        ..Default::default()
                    },
                    fkernel::PerCpuStats {
                        flags: Some(0),
                        idle_time: Some(2_000_000_000),
                        ..Default::default()
                    },
                ],
                vec![
                    fkernel::PerCpuStats {
                        flags: Some(1),
                        idle_time: Some(1_500_000_000),
                        active_energy_consumption_nj: Some(0),
                        idle_energy_consumption_nj: Some(0),
                        ..Default::default()
                    },
                    fkernel::PerCpuStats {
                        flags: Some(0),
                        idle_time: Some(2_000_000_000),
                        ..Default::default()
                    },
                ],
                vec![
                    fkernel::PerCpuStats {
                        flags: Some(1),
                        idle_time: Some(1_500_000_000),
                        ..Default::default()
                    },
                    fkernel::PerCpuStats {
                        flags: Some(0),
                        idle_time: Some(2_000_000_000),
                        ..Default::default()
                    },
                ],
                vec![
                    fkernel::PerCpuStats {
                        flags: Some(1),
                        idle_time: Some(2_000_000_000),
                        active_energy_consumption_nj: Some(500_000_000),
                        ..Default::default()
                    },
                    fkernel::PerCpuStats {
                        flags: Some(0),
                        idle_time: Some(2_000_000_000),
                        ..Default::default()
                    },
                ],
                vec![
                    fkernel::PerCpuStats {
                        flags: Some(1),
                        idle_time: Some(3_000_000_000),
                        active_energy_consumption_nj: Some(500_000_000),
                        ..Default::default()
                    },
                    fkernel::PerCpuStats {
                        flags: Some(0),
                        idle_time: Some(2_000_000_000),
                        ..Default::default()
                    },
                ],
            ];
            let mut idx = 0_usize;
            while let Some(Ok(request)) = stream.next().await {
                if let fkernel::StatsRequest::GetCpuStats { responder } = request {
                    let per_cpu_stats = Some(samples[idx.min(samples.len() - 1)].clone());
                    idx += 1;
                    let _ =
                        responder.send(&fkernel::CpuStats { actual_num_cpus: 2, per_cpu_stats });
                }
            }
        });

        let mut collector = CpuCollector::with_proxy(Some(proxy));
        let unprimed = collector.collect_at(mono(1_000_000_000)).await;
        assert_eq!(unprimed.avg_load_pct, None);
        assert!(unprimed.per_core_load_pct.is_empty());
        assert_eq!(unprimed.power_mw, None);

        let sample = collector.collect_at(mono(2_000_000_000)).await;
        assert_eq!(sample.per_core_load_pct, vec![50, 0]);
        assert!((sample.avg_load_pct.expect("online avg") - 50.0).abs() < 1e-4);
        assert_eq!(sample.power_mw, None);

        let zero_delta = collector.collect_at(mono(2_000_000_000)).await;
        assert_eq!(zero_delta, sample);

        let with_energy = collector.collect_at(mono(3_000_000_000)).await;
        assert_eq!(with_energy.power_mw, Some(500));

        let quiet_energy = collector.collect_at(mono(4_000_000_000)).await;
        assert_eq!(quiet_energy.power_mw, Some(0));
    }

    #[fuchsia::test]
    async fn test_memory_collector_partial_stats_and_closed_proxy() {
        let scope = fasync::Scope::new();
        let (proxy, mut stream) = create_proxy_and_stream::<fkernel::StatsMarker>();

        scope.spawn(async move {
            let mut call = 0_u32;
            while let Some(Ok(request)) = stream.next().await {
                match request {
                    fkernel::StatsRequest::GetMemoryStats { responder } => {
                        call += 1;
                        let mem = if call == 1 {
                            fkernel::MemoryStats {
                                total_bytes: Some(2048 * MB),
                                free_bytes: None,
                                ..Default::default()
                            }
                        } else {
                            fkernel::MemoryStats {
                                total_bytes: Some(1024 * MB),
                                free_bytes: Some(2048 * MB),
                                ..Default::default()
                            }
                        };
                        let _ = responder.send(&mem);
                    }
                    fkernel::StatsRequest::GetMemoryStatsCompression { responder } => {
                        let _ = responder.send(&fkernel::MemoryStatsCompression::default());
                        if call >= 2 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });

        let mut collector = MemoryCollector::with_proxy(Some(proxy));
        let partial = collector.collect().await;
        assert_eq!(partial.total_mb, Some(2048));
        assert_eq!(partial.free_mb, None);
        assert_eq!(partial.used_mb, None);

        let saturated = collector.collect().await;
        assert_eq!(saturated.used_mb, Some(0));

        let closed = collector.collect().await;
        assert_eq!(closed, MemoryMetrics::default());
    }

    #[fuchsia::test]
    async fn test_gpu_collector_lazy_selection_and_counter_reset() {
        let scope = fasync::Scope::new();
        let (unsupported_proxy, mut unsupported_stream) =
            create_proxy_and_stream::<fgpu::DeviceMarker>();
        let (supported_proxy, mut supported_stream) =
            create_proxy_and_stream::<fgpu::DeviceMarker>();

        scope.spawn(async move {
            while let Some(Ok(request)) = unsupported_stream.next().await {
                if let fgpu::DeviceRequest::Query { query_id, responder } = request {
                    assert_eq!(query_id, fgpu::QueryId::IsTotalTimeSupported);
                    let _ = responder.send(Ok(fgpu::DeviceQueryResponse::SimpleResult(0)));
                }
            }
        });

        scope.spawn(async move {
            let samples = [
                (1_000_000_000_u64, 5_000_000_000_u64),
                (1_250_000_000_u64, 6_000_000_000_u64),
                (1_250_000_000_u64, 6_000_000_000_u64),
                (100_000_000_u64, 7_000_000_000_u64),
                (600_000_000_u64, 8_000_000_000_u64),
            ];
            let mut idx = 0_usize;
            while let Some(Ok(request)) = supported_stream.next().await {
                if let fgpu::DeviceRequest::Query { query_id, responder } = request {
                    match query_id {
                        fgpu::QueryId::IsTotalTimeSupported => {
                            let _ = responder.send(Ok(fgpu::DeviceQueryResponse::SimpleResult(1)));
                        }
                        fgpu::QueryId::MagmaQueryTotalTime => {
                            let (gpu_ns, mono_ns) = samples[idx.min(samples.len() - 1)];
                            idx += 1;
                            let vmo = make_magma_time_vmo(gpu_ns, mono_ns);
                            let _ =
                                responder.send(Ok(fgpu::DeviceQueryResponse::BufferResult(vmo)));
                        }
                        _ => {}
                    }
                }
            }
        });

        let mut unsupported_paths = Vec::new();
        let mut connected_paths = Vec::new();
        let mut unsupported_slot = Some(unsupported_proxy);
        let mut supported_slot = Some(supported_proxy);
        let selected = GpuCollector::select_supported_device(
            vec![
                "/svc/fuchsia.gpu.magma.TrustedService/npu/device".to_string(),
                "/svc/fuchsia.gpu.magma.Service/gpu/device".to_string(),
                "/svc/fuchsia.gpu.magma.Service/extra/device".to_string(),
            ],
            |path| {
                connected_paths.push(path.to_string());
                match path {
                    "/svc/fuchsia.gpu.magma.TrustedService/npu/device" => unsupported_slot.take(),
                    "/svc/fuchsia.gpu.magma.Service/gpu/device" => supported_slot.take(),
                    _ => panic!("should not connect after finding supported device"),
                }
            },
            &mut unsupported_paths,
        )
        .await;
        assert_eq!(connected_paths.len(), 2);
        assert!(selected.is_some());

        let mut collector = GpuCollector::with_proxy(selected);
        collector.prime_baseline().await;

        let gpu = collector.collect().await;
        assert!((gpu.load_pct.expect("gpu load sample") - 25.0).abs() < 1e-4);

        let cached = collector.collect().await;
        assert!((cached.load_pct.expect("cached gpu load") - 25.0).abs() < 1e-4);

        let rebased = collector.collect().await;
        assert_eq!(rebased.load_pct, None);

        let recovered = collector.collect().await;
        assert!((recovered.load_pct.expect("recovered gpu load") - 50.0).abs() < 1e-4);
    }

    #[fuchsia::test]
    async fn test_power_collector_battery_and_suspend_residency() {
        let scope = fasync::Scope::new();
        let (proxy, mut stream) = create_proxy_and_stream::<fbattery::BatteryManagerMarker>();

        scope.spawn(async move {
            let mut call = 0_u32;
            while let Some(Ok(request)) = stream.next().await {
                if let fbattery::BatteryManagerRequest::GetBatteryInfo { responder } = request {
                    call += 1;
                    let info = if call == 1 {
                        fbattery::BatteryInfo {
                            level_percent: Some(78.0),
                            charge_status: Some(fbattery::ChargeStatus::Discharging),
                            charge_source: Some(fbattery::ChargeSource::None),
                            temperature_mc: Some(29_500),
                            present_voltage_mv: Some(3850),
                            present_charging_current_ua: Some(-420_000),
                            average_charging_current_ua: Some(-400_000),
                            ..Default::default()
                        }
                    } else {
                        fbattery::BatteryInfo {
                            level_percent: Some(f32::NAN),
                            present_voltage_mv: Some(u32::MAX),
                            present_charging_current_ua: Some(i32::MIN),
                            ..Default::default()
                        }
                    };
                    let _ = responder.send(&info);
                    if call >= 2 {
                        break;
                    }
                }
            }
        });

        let mut collector = PowerCollector::with_proxy(Some(proxy));
        let power = collector.collect_at(mono(600_000_000_000), boot(1_000_000_000_000)).await;

        assert_eq!(power.battery_percent, Some(78.0));
        assert_eq!(power.charge_status, Some(ChargeStatus::Discharging));
        assert_eq!(power.charge_status.map(ChargeStatus::as_str), Some("Discharging"));
        assert_eq!(power.charge_source, Some(ChargeSource::None));
        assert_eq!(power.charge_source.map(ChargeSource::as_str), Some("None"));
        assert!((power.battery_temp_c.expect("temp c") - 29.5).abs() < 1e-4);
        assert_eq!(power.voltage_mv, Some(3850));
        assert_eq!(power.current_ma, Some(-420));
        assert_eq!(power.avg_current_ma, Some(-400));
        assert_eq!(power.power_mw, Some(-1617));
        assert_eq!(power.mono_uptime_secs, 600);
        assert_eq!(power.boot_uptime_secs, 1000);
        assert!((power.suspend_residency_pct - 40.0).abs() < 1e-4);

        let clamped = collector.collect_at(mono(500_000_000_000), boot(499_000_000_000)).await;
        assert_eq!(clamped.battery_percent, None);
        assert_eq!(clamped.power_mw, Some(i32::MIN));
        assert_eq!(clamped.charge_status, None);
        assert_eq!(clamped.charge_source, None);
        assert_eq!(clamped.suspend_residency_pct, 0.0);

        let disconnected = collector.collect_at(mono(500_000_000_000), boot(500_000_000_000)).await;
        assert_eq!(disconnected.charge_status, None);
        assert_eq!(disconnected.power_mw, None);
    }

    #[fuchsia::test]
    async fn test_power_collector_hardware_battery_fallback_and_smoothing() {
        let scope = fasync::Scope::new();
        let (closed_mgr_proxy, closed_mgr_stream) =
            create_proxy_and_stream::<fbattery::BatteryManagerMarker>();
        drop(closed_mgr_stream);
        let (hw_proxy, mut hw_stream) = create_proxy_and_stream::<fhwbattery::BatteryMarker>();

        scope.spawn(async move {
            let statuses = [
                fhwbattery::Status {
                    present: Some(true),
                    level_percent: Some(64.5),
                    voltage_uv: Some(4_000_000),
                    current_ua: Some(-300_000),
                    temp_celsius: Some(31.25),
                    ..Default::default()
                },
                fhwbattery::Status {
                    present: Some(true),
                    level_percent: Some(64.0),
                    voltage_uv: Some(4_000_000),
                    current_ua: Some(-600_000),
                    temp_celsius: Some(31.5),
                    ..Default::default()
                },
                fhwbattery::Status {
                    present: Some(false),
                    level_percent: Some(50.0),
                    voltage_uv: Some(3_900_000),
                    current_ua: Some(-100_000),
                    ..Default::default()
                },
            ];
            let mut idx = 0_usize;
            while let Some(Ok(request)) = hw_stream.next().await {
                if let fhwbattery::BatteryRequest::GetStatus { responder } = request {
                    let status = statuses[idx.min(statuses.len() - 1)].clone();
                    idx += 1;
                    let _ = responder.send(Ok(&status));
                }
            }
        });

        let mut collector = PowerCollector::with_proxies(Some(closed_mgr_proxy), Some(hw_proxy));
        // Closed BatteryManager falls through immediately to hardware battery on the same sample.
        let first = collector.collect_at(mono(100_000_000_000), boot(125_000_000_000)).await;
        assert_eq!(first.battery_percent, Some(64.5));
        assert_eq!(first.charge_status, None);
        assert_eq!(first.charge_source, None);
        assert_eq!(first.voltage_mv, Some(4000));
        assert_eq!(first.current_ma, Some(-300));
        assert_eq!(first.avg_current_ma, Some(-300));
        assert_eq!(first.power_mw, Some(-1200));
        assert!((first.battery_temp_c.expect("hw temp") - 31.25).abs() < 1e-4);

        // Second sample updates 1/3 EMA: -300 + (1/3)*(-600 - -300) = -400 mA.
        let second = collector.collect_at(mono(101_000_000_000), boot(126_000_000_000)).await;
        assert_eq!(second.current_ma, Some(-600));
        assert_eq!(second.avg_current_ma, Some(-400));
        assert_eq!(second.power_mw, Some(-2400));

        // Detached pack (present == Some(false)) clears battery fields and resets EMA.
        let detached = collector.collect_at(mono(102_000_000_000), boot(127_000_000_000)).await;
        assert_eq!(detached.battery_percent, None);
        assert_eq!(detached.voltage_mv, None);
        assert_eq!(detached.current_ma, None);
        assert_eq!(detached.avg_current_ma, None);
        assert_eq!(detached.power_mw, None);
        assert_eq!(detached.mono_uptime_secs, 102);
    }

    #[fuchsia::test]
    fn test_scenic_collector_hierarchy_custom_selector_and_fps() {
        let hierarchy = DiagnosticsHierarchy::new(
            "root",
            vec![],
            vec![DiagnosticsHierarchy::new(
                "scenic",
                vec![],
                vec![DiagnosticsHierarchy::new(
                    "FrameScheduler",
                    vec![Property::Uint("most_recent_frame_number".to_string(), 3483)],
                    vec![],
                )],
            )],
        );
        assert_eq!(
            scenic::extract_frame_number_with_selector(
                &hierarchy,
                &scenic::PropertySelectorKind::DefaultScenicPath,
            ),
            Some(3483)
        );

        let custom_hierarchy = DiagnosticsHierarchy::new(
            "root",
            vec![],
            vec![DiagnosticsHierarchy::new(
                "custom",
                vec![Property::Int("frames".to_string(), 912)],
                vec![],
            )],
        );
        let exact_raw = "core/ui/scenic:root/custom:frames";
        let exact_parsed = selectors::parse_verbose(exact_raw).expect("valid exact selector");
        let exact_kind = scenic::PropertySelectorKind::from_parsed(exact_raw, &exact_parsed);
        assert!(matches!(exact_kind, scenic::PropertySelectorKind::ExactPath { .. }));
        assert_eq!(
            scenic::extract_frame_number_with_selector(&custom_hierarchy, &exact_kind),
            Some(912)
        );

        let wildcard_raw = "core/ui/scenic:root/*:frames";
        let wildcard_parsed =
            selectors::parse_verbose(wildcard_raw).expect("valid wildcard selector");
        let wildcard_kind =
            scenic::PropertySelectorKind::from_parsed(wildcard_raw, &wildcard_parsed);
        assert!(matches!(wildcard_kind, scenic::PropertySelectorKind::Wildcard(_)));
        assert_eq!(
            scenic::extract_frame_number_with_selector(&custom_hierarchy, &wildcard_kind),
            Some(912)
        );

        let mut collector = ScenicCollector::disconnected();
        let first = collector.record_frame_sample(1_000_000_000, 3423);
        assert_eq!(first, ScenicMetrics { frame_count: Some(3423), fps: None });

        let sub_window = collector.record_frame_sample(1_015_000_000, 3424);
        assert_eq!(sub_window, ScenicMetrics { frame_count: Some(3424), fps: None });

        let second = collector.record_frame_sample(2_000_000_000, 3483);
        assert_eq!(second, ScenicMetrics { frame_count: Some(3483), fps: Some(60) });

        let stale = collector.record_frame_sample(6_000_000_000, 3543);
        assert_eq!(stale, ScenicMetrics { frame_count: Some(3543), fps: None });

        let reset = collector.record_frame_sample(7_000_000_000, 100);
        assert_eq!(reset, ScenicMetrics { frame_count: Some(100), fps: None });
        let after_reset = collector.record_frame_sample(8_000_000_000, 160);
        assert_eq!(after_reset, ScenicMetrics { frame_count: Some(160), fps: Some(60) });
    }

    #[fuchsia::test]
    async fn test_scenic_collector_archive_accessor_and_cache_hit_prime_baseline() {
        let scope = fasync::Scope::new();
        let (proxy, mut stream) = create_proxy_and_stream::<fdiagnostics::ArchiveAccessorMarker>();

        scope.spawn(async move {
            let frames = [100_u64, 163_u64];
            let mut snapshot_idx = 0_usize;
            while let Some(Ok(request)) = stream.next().await {
                match request {
                    fdiagnostics::ArchiveAccessorRequest::StreamDiagnostics {
                        result_stream,
                        ..
                    } => {
                        let frame = frames[snapshot_idx.min(frames.len() - 1)];
                        snapshot_idx += 1;
                        let mut batch_stream = result_stream.into_stream();
                        let mut sent = false;
                        while let Some(Ok(batch_req)) = batch_stream.next().await {
                            match batch_req {
                                fdiagnostics::BatchIteratorRequest::WaitForReady { responder } => {
                                    let _ = responder.send();
                                }
                                fdiagnostics::BatchIteratorRequest::GetNext { responder } => {
                                    if sent {
                                        let _ = responder.send(Ok(Vec::new()));
                                        continue;
                                    }
                                    sent = true;
                                    let json = format!(
                                        "[{{\"data_source\":\"Inspect\",\"metadata\":{{\"component_url\":\"fuchsia-pkg://fuchsia.com/scenic#meta/scenic.cm\",\"filename\":\"fuchsia.inspect.Tree\",\"timestamp\":0}},\"moniker\":\"core/ui/scenic\",\"payload\":{{\"root\":{{\"scenic\":{{\"FrameScheduler\":{{\"most_recent_frame_number\":{frame}}}}}}}}},\"version\":1}}]"
                                    );
                                    let size = json.len() as u64;
                                    let vmo = zx::Vmo::create(size).expect("create inspect vmo");
                                    vmo.write(json.as_bytes(), 0).expect("write inspect vmo");
                                    let buffer = fidl_fuchsia_mem::Buffer { vmo, size };
                                    let _ = responder.send(Ok(vec![
                                        fdiagnostics::FormattedContent::Json(buffer),
                                    ]));
                                }
                                _ => {}
                            }
                        }
                    }
                    fdiagnostics::ArchiveAccessorRequest::WaitForReady { responder, .. } => {
                        let _ = responder.send();
                    }
                    _ => {}
                }
            }
        });

        let mut collector = ScenicCollector::with_archive_accessor(Some(proxy));
        let initial = collector.collect_at(mono(1_000_000_000)).await;
        assert_eq!(initial, ScenicMetrics { frame_count: Some(100), fps: None });

        // Prime 50ms later within the 100ms cache window: must retain the t = 1.000s timestamp.
        collector.prime_baseline_at(mono(1_050_000_000)).await;

        // 63 frames over 1.050s -> 60 FPS (not 63 FPS).
        let next = collector.collect_at(mono(2_050_000_000)).await;
        assert_eq!(next, ScenicMetrics { frame_count: Some(163), fps: Some(60) });
    }

    #[fuchsia::test]
    async fn test_diagnostics_collector_end_to_end_caching_and_clone_from() {
        let mut collector = DiagnosticsCollector::disconnected();
        assert!(collector.cached_snapshot().is_none());

        let cold = collector.snapshot(CachePolicy::UseCached).await.clone();
        assert_eq!(cold.generation, 1);
        assert_eq!(collector.cached_snapshot().map(|s| s.generation), Some(1));

        let cached = collector.snapshot(CachePolicy::UseCached).await.clone();
        assert_eq!(cached.generation, 1);

        let refreshed = collector.snapshot(CachePolicy::ForceRefresh).await.clone();
        assert_eq!(refreshed.generation, 2);

        let mut reused = DiagnosticSnapshot::default();
        reused.cpu.per_core_load_pct = Vec::with_capacity(16);
        let initial_ptr = reused.cpu.per_core_load_pct.as_ptr();
        let mut source = refreshed.clone();
        source.cpu.per_core_load_pct = vec![10, 20, 30, 40];
        reused.clone_from(&source);
        assert_eq!(reused.cpu.per_core_load_pct.as_ptr(), initial_ptr);
        assert_eq!(reused, source);

        let timed = collector.collect_at(mono(600_000_000_000), boot(1_000_000_000_000)).await;
        assert_eq!(timed.generation, 3);
        assert_eq!(timed.power.mono_uptime_secs, 600);
        assert_eq!(timed.power.boot_uptime_secs, 1000);
        assert!((timed.power.suspend_residency_pct - 40.0).abs() < 1e-4);
        assert_eq!(collector.cached_snapshot().map(|s| s.generation), Some(3));

        collector.prime_baseline().await;
        let collected = collector.collect().await;
        assert_eq!(collected.generation, 4);
    }

    #[fuchsia::test]
    async fn test_diagnostics_collector_builder_selective_sources() {
        let (proxy, _stream) = create_proxy_and_stream::<fkernel::StatsMarker>();
        let (hw_proxy, _hw_stream) = create_proxy_and_stream::<fhwbattery::BatteryMarker>();
        let headless = DiagnosticsCollector::builder()
            .with_cpu()
            .with_memory()
            .with_power()
            .build_with_connect(
                || Some(proxy),
                || panic!("gpu should not connect when disabled"),
                || PowerCollector::with_hardware_proxy(Some(hw_proxy)),
                || panic!("scenic should not connect when disabled"),
            );
        assert!(headless.cpu.is_connected());
        assert!(headless.memory.is_connected());
        assert!(headless.shares_kernel_stats);
        assert!(headless.power.is_enabled());
        assert!(!headless.gpu.is_enabled());
        assert!(!headless.scenic.is_enabled());
    }
}
