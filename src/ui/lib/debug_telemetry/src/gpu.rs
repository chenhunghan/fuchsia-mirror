// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Magma GPU utilization telemetry collection via `fuchsia.gpu.magma.Device`.

use fidl_fuchsia_gpu_magma as fgpu;
use fuchsia_component::client::connect_to_protocol_at_path;
use fuchsia_fs::PERM_READABLE;
use fuchsia_fs::directory::{open_in_namespace, readdir};

const MAGMA_TOTAL_TIME_BUFFER_SIZE: usize = 16;
const FULL_PERCENT: f64 = 100.0;
const INITIAL_DISCOVERY_BACKOFF_NS: i64 = 1_000_000_000;
const MAX_DISCOVERY_BACKOFF_NS: i64 = 60_000_000_000;

/// Ordered so `fuchsia.gpu.magma.Service` (exported by GPU MSDs, not NPU-only MSDs) is probed
/// before `TrustedService` and `/dev/class/gpu`.
const MAGMA_SERVICE_DIRS: [(&str, &str); 3] = [
    ("/svc/fuchsia.gpu.magma.Service", "/device"),
    ("/svc/fuchsia.gpu.magma.TrustedService", "/device"),
    ("/dev/class/gpu", ""),
];
const ALL_MAGMA_DIRS_MASK: u8 = (1_u8 << MAGMA_SERVICE_DIRS.len()) - 1;

/// Snapshot of active GPU utilization over the latest sample window.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
pub struct GpuMetrics {
    /// Percentage (`0.0..=100.0`) of elapsed monotonic time during which the GPU was active.
    pub load_pct: Option<f32>,
}

enum QueryError {
    ChannelClosed,
    UnsupportedOrFailed,
}

/// Collects GPU active utilization percentage from `fuchsia.gpu.magma.Device` using
/// `QueryId::MagmaQueryTotalTime`.
#[derive(Debug)]
pub struct GpuCollector {
    device: Option<fgpu::DeviceProxy>,
    auto_reconnect: bool,
    missing_dir_mask: u8,
    unsupported_paths: Vec<String>,
    next_discovery_ns: Option<i64>,
    discovery_backoff_ns: i64,
    previous_sample: Option<(u64, u64)>,
    last_load_pct: Option<f32>,
}

impl GpuCollector {
    /// Creates a `GpuCollector` that lazily discovers the first Magma GPU device supporting
    /// `MagmaQueryTotalTime` in the incoming namespace on the first async sample.
    pub fn from_namespace() -> Self {
        Self {
            device: None,
            auto_reconnect: true,
            missing_dir_mask: 0,
            unsupported_paths: Vec::new(),
            next_discovery_ns: None,
            discovery_backoff_ns: INITIAL_DISCOVERY_BACKOFF_NS,
            previous_sample: None,
            last_load_pct: None,
        }
    }

    /// Creates an inert collector with no Magma `DeviceProxy` and discovery disabled.
    pub fn disconnected() -> Self {
        Self::with_proxy(None)
    }

    /// Creates a collector backed by an explicit Magma `DeviceProxy` without filesystem discovery.
    pub fn with_proxy(device: Option<fgpu::DeviceProxy>) -> Self {
        Self {
            device,
            auto_reconnect: false,
            missing_dir_mask: 0,
            unsupported_paths: Vec::new(),
            next_discovery_ns: None,
            discovery_backoff_ns: INITIAL_DISCOVERY_BACKOFF_NS,
            previous_sample: None,
            last_load_pct: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_enabled(&self) -> bool {
        self.device.is_some() || self.auto_reconnect
    }

    pub(crate) async fn is_total_time_supported(proxy: &fgpu::DeviceProxy) -> bool {
        matches!(
            proxy.query(fgpu::QueryId::IsTotalTimeSupported).await,
            Ok(Ok(fgpu::DeviceQueryResponse::SimpleResult(1)))
        )
    }

    pub(crate) async fn select_supported_device<I, F>(
        candidate_paths: I,
        mut connect: F,
        unsupported_paths: &mut Vec<String>,
    ) -> Option<fgpu::DeviceProxy>
    where
        I: IntoIterator<Item = String>,
        F: FnMut(&str) -> Option<fgpu::DeviceProxy>,
    {
        for path in candidate_paths {
            if unsupported_paths.iter().any(|unsupported| unsupported == &path) {
                continue;
            }
            let Some(proxy) = connect(&path) else {
                continue;
            };
            if Self::is_total_time_supported(&proxy).await {
                return Some(proxy);
            }
            unsupported_paths.push(path);
        }
        None
    }

    async fn ensure_device(&mut self) {
        if self.device.is_some()
            || !self.auto_reconnect
            || (self.missing_dir_mask & ALL_MAGMA_DIRS_MASK) == ALL_MAGMA_DIRS_MASK
        {
            return;
        }

        let now_ns = zx::MonotonicInstant::get().into_nanos().max(0);
        if self.next_discovery_ns.is_some_and(|next_ns| now_ns < next_ns) {
            return;
        }

        for (dir_idx, (directory, suffix)) in MAGMA_SERVICE_DIRS.into_iter().enumerate() {
            let dir_bit = 1_u8 << dir_idx;
            if (self.missing_dir_mask & dir_bit) != 0 {
                continue;
            }
            let Ok(dir_proxy) = open_in_namespace(directory, PERM_READABLE) else {
                self.missing_dir_mask |= dir_bit;
                continue;
            };
            let Ok(entries) = readdir(&dir_proxy).await else {
                self.missing_dir_mask |= dir_bit;
                continue;
            };

            let candidate_paths = entries.into_iter().filter_map(|entry| {
                if entry.name.is_empty() || entry.name.starts_with('.') {
                    return None;
                }
                Some(format!("{directory}/{}{suffix}", entry.name))
            });

            if let Some(proxy) = Self::select_supported_device(
                candidate_paths,
                |path| connect_to_protocol_at_path::<fgpu::DeviceMarker>(path).ok(),
                &mut self.unsupported_paths,
            )
            .await
            {
                self.device = Some(proxy);
                self.next_discovery_ns = None;
                self.discovery_backoff_ns = INITIAL_DISCOVERY_BACKOFF_NS;
                return;
            }
        }

        if (self.missing_dir_mask & ALL_MAGMA_DIRS_MASK) != ALL_MAGMA_DIRS_MASK {
            self.next_discovery_ns = Some(now_ns.saturating_add(self.discovery_backoff_ns));
            self.discovery_backoff_ns =
                self.discovery_backoff_ns.saturating_mul(2).min(MAX_DISCOVERY_BACKOFF_NS);
        }
    }

    fn handle_disconnect(&mut self) {
        self.device = None;
        self.previous_sample = None;
        self.last_load_pct = None;
        if self.auto_reconnect {
            self.missing_dir_mask = 0;
            self.next_discovery_ns = None;
            self.discovery_backoff_ns = INITIAL_DISCOVERY_BACKOFF_NS;
        }
    }

    async fn query_magma_total_time(proxy: &fgpu::DeviceProxy) -> Result<(u64, u64), QueryError> {
        let vmo = match proxy.query(fgpu::QueryId::MagmaQueryTotalTime).await {
            Ok(Ok(fgpu::DeviceQueryResponse::BufferResult(vmo))) => vmo,
            Ok(_) => return Err(QueryError::UnsupportedOrFailed),
            Err(err) if err.is_closed() => return Err(QueryError::ChannelClosed),
            Err(_) => return Err(QueryError::UnsupportedOrFailed),
        };
        let mut buffer = [0_u8; MAGMA_TOTAL_TIME_BUFFER_SIZE];
        vmo.read(&mut buffer, 0).map_err(|_| QueryError::UnsupportedOrFailed)?;
        let gpu_time_ns = u64::from_le_bytes(
            buffer[0..8].try_into().map_err(|_| QueryError::UnsupportedOrFailed)?,
        );
        let monotonic_time_ns = u64::from_le_bytes(
            buffer[8..16].try_into().map_err(|_| QueryError::UnsupportedOrFailed)?,
        );
        Ok((gpu_time_ns, monotonic_time_ns))
    }

    /// Captures a baseline `(gpu_time_ns, monotonic_time_ns)` sample for subsequent utilization
    /// delta computation.
    pub async fn prime_baseline(&mut self) {
        self.ensure_device().await;
        let Some(proxy) = self.device.as_ref() else {
            return;
        };
        match Self::query_magma_total_time(proxy).await {
            Ok(sample) => self.previous_sample = Some(sample),
            Err(QueryError::ChannelClosed) => self.handle_disconnect(),
            Err(QueryError::UnsupportedOrFailed) => {
                // Disable further polling on a device whose total-time query fails at runtime.
                self.device = None;
                self.auto_reconnect = false;
            }
        }
    }

    /// Collects [`GpuMetrics`] (`load_pct` in `0.0..=100.0`) since the previous sample.
    pub async fn collect(&mut self) -> GpuMetrics {
        self.ensure_device().await;
        let Some(proxy) = self.device.as_ref() else {
            return GpuMetrics::default();
        };
        let (current_gpu_ns, current_monotonic_ns) = match Self::query_magma_total_time(proxy).await
        {
            Ok(sample) => sample,
            Err(QueryError::ChannelClosed) => {
                self.handle_disconnect();
                return GpuMetrics::default();
            }
            Err(QueryError::UnsupportedOrFailed) => {
                self.device = None;
                self.auto_reconnect = false;
                return GpuMetrics::default();
            }
        };

        let load_pct = if let Some((previous_gpu_ns, previous_monotonic_ns)) = self.previous_sample
        {
            match (
                current_gpu_ns.checked_sub(previous_gpu_ns),
                current_monotonic_ns.checked_sub(previous_monotonic_ns),
            ) {
                (Some(gpu_delta_ns), Some(monotonic_delta_ns)) if monotonic_delta_ns > 0 => {
                    let pct = ((gpu_delta_ns as f64 / monotonic_delta_ns as f64) * FULL_PERCENT)
                        .clamp(0.0, FULL_PERCENT) as f32;
                    self.previous_sample = Some((current_gpu_ns, current_monotonic_ns));
                    self.last_load_pct = Some(pct);
                    Some(pct)
                }
                (Some(_), Some(0)) => self.last_load_pct,
                _ => {
                    // Counter reset (e.g., driver restart): rebase without reporting a bogus 0%.
                    self.previous_sample = Some((current_gpu_ns, current_monotonic_ns));
                    self.last_load_pct = None;
                    None
                }
            }
        } else {
            self.previous_sample = Some((current_gpu_ns, current_monotonic_ns));
            None
        };

        GpuMetrics { load_pct }
    }
}
