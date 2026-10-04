// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Structured configuration and runtime limit bounds (Spec Section 22).
//!
//! Structured configuration can narrow (reduce) bounds such as enabled state,
//! audit capacity, maximum operations, and maximum deadlines. It may never
//! widen any bound beyond the baseline specification.

use crate::access_policy::{AccessClass, Denial};
use std::collections::BTreeMap;

/// Baseline upper bounds enforced by the core proxy specification.
/// Structured configuration can only narrow (reduce) these bounds.
pub const BASELINE_AUDIT_CAPACITY: u32 = 1024;
pub const BASELINE_MAX_SNAPSHOT_ITEMS: u32 = 64;
pub const BASELINE_MAX_SEQUENCE_ITEMS: u32 = 64;
pub const BASELINE_MAX_DELAY_NS: u64 = 1_000_000_000;
pub const BASELINE_MAX_SEQUENCE_DURATION_NS: u64 = 1_000_000_000;
pub const BASELINE_MAX_DEADLINE_NS: u64 = 1_000_000_000;

/// Structured configuration values for the proxy driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProxyConfig {
    /// Whether the proxy accepts sessions. If false, all sessions are rejected.
    pub enabled: bool,
    /// Capacity of the audit ring buffer. Can only narrow (<= 1024).
    pub audit_capacity: u32,
    /// Maximum items allowed in a single snapshot. Can only narrow (<= 64).
    pub max_snapshot_items: u32,
    /// Maximum items allowed in a single sequence. Can only narrow (<= 64).
    pub max_sequence_items: u32,
    /// Maximum delay allowed per sequence step in ns (<= 100ms).
    pub max_delay_ns: u64,
    /// Maximum sequence duration in ns (<= 1s).
    pub max_sequence_duration_ns: u64,
    /// Maximum deadline timeout allowed for waits and polls in ns (<= 1s).
    pub max_deadline_ns: u64,
    /// Maximum operations per second per session (0 = unlimited).
    pub max_ops_per_second: u32,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            audit_capacity: BASELINE_AUDIT_CAPACITY,
            max_snapshot_items: BASELINE_MAX_SNAPSHOT_ITEMS,
            max_sequence_items: BASELINE_MAX_SEQUENCE_ITEMS,
            max_delay_ns: BASELINE_MAX_DELAY_NS,
            max_sequence_duration_ns: BASELINE_MAX_SEQUENCE_DURATION_NS,
            max_deadline_ns: BASELINE_MAX_DEADLINE_NS,
            max_ops_per_second: 0,
        }
    }
}

/// Why a runtime configuration failed narrowing validation against baseline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigNarrowingError {
    WidenedEnabledState,
    WidenedAuditCapacity { configured: u32, baseline: u32 },
    WidenedMaxSnapshotItems { configured: u32, baseline: u32 },
    WidenedMaxSequenceItems { configured: u32, baseline: u32 },
    WidenedMaxDelay { configured: u64, baseline: u64 },
    WidenedMaxSequenceDuration { configured: u64, baseline: u64 },
    WidenedMaxDeadline { configured: u64, baseline: u64 },
    WidenedMaxOpsPerSecond { configured: u32, baseline: u32 },
}

impl std::fmt::Display for ConfigNarrowingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WidenedEnabledState => write!(f, "cannot enable proxy when baseline is disabled"),
            Self::WidenedAuditCapacity { configured, baseline } => {
                write!(f, "audit_capacity {configured} exceeds baseline {baseline}")
            }
            Self::WidenedMaxSnapshotItems { configured, baseline } => {
                write!(f, "max_snapshot_items {configured} exceeds baseline {baseline}")
            }
            Self::WidenedMaxSequenceItems { configured, baseline } => {
                write!(f, "max_sequence_items {configured} exceeds baseline {baseline}")
            }
            Self::WidenedMaxDelay { configured, baseline } => {
                write!(f, "max_delay_ns {configured} exceeds baseline {baseline}")
            }
            Self::WidenedMaxSequenceDuration { configured, baseline } => {
                write!(f, "max_sequence_duration_ns {configured} exceeds baseline {baseline}")
            }
            Self::WidenedMaxDeadline { configured, baseline } => {
                write!(f, "max_deadline_ns {configured} exceeds baseline {baseline}")
            }
            Self::WidenedMaxOpsPerSecond { configured, baseline } => {
                write!(f, "max_ops_per_second {configured} widens baseline limit {baseline}")
            }
        }
    }
}

impl std::error::Error for ConfigNarrowingError {}

impl ProxyConfig {
    /// Validates that `runtime` narrows or preserves this baseline config.
    pub fn narrow_with(&self, runtime: &Self) -> Result<Self, ConfigNarrowingError> {
        if !self.enabled && runtime.enabled {
            return Err(ConfigNarrowingError::WidenedEnabledState);
        }
        if runtime.audit_capacity > self.audit_capacity {
            return Err(ConfigNarrowingError::WidenedAuditCapacity {
                configured: runtime.audit_capacity,
                baseline: self.audit_capacity,
            });
        }
        if runtime.max_snapshot_items > self.max_snapshot_items {
            return Err(ConfigNarrowingError::WidenedMaxSnapshotItems {
                configured: runtime.max_snapshot_items,
                baseline: self.max_snapshot_items,
            });
        }
        if runtime.max_sequence_items > self.max_sequence_items {
            return Err(ConfigNarrowingError::WidenedMaxSequenceItems {
                configured: runtime.max_sequence_items,
                baseline: self.max_sequence_items,
            });
        }
        if runtime.max_delay_ns > self.max_delay_ns {
            return Err(ConfigNarrowingError::WidenedMaxDelay {
                configured: runtime.max_delay_ns,
                baseline: self.max_delay_ns,
            });
        }
        if runtime.max_sequence_duration_ns > self.max_sequence_duration_ns {
            return Err(ConfigNarrowingError::WidenedMaxSequenceDuration {
                configured: runtime.max_sequence_duration_ns,
                baseline: self.max_sequence_duration_ns,
            });
        }
        if runtime.max_deadline_ns > self.max_deadline_ns {
            return Err(ConfigNarrowingError::WidenedMaxDeadline {
                configured: runtime.max_deadline_ns,
                baseline: self.max_deadline_ns,
            });
        }
        if self.max_ops_per_second > 0
            && (runtime.max_ops_per_second == 0
                || runtime.max_ops_per_second > self.max_ops_per_second)
        {
            return Err(ConfigNarrowingError::WidenedMaxOpsPerSecond {
                configured: runtime.max_ops_per_second,
                baseline: self.max_ops_per_second,
            });
        }
        Ok(*runtime)
    }
}

/// Tracks access counts and timestamps to enforce access-class count/rate limits (Spec 12.1 step 9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimiter {
    max_ops_per_second: u32,
    window_start_ns: i64,
    ops_in_window: u32,
}

impl RateLimiter {
    pub fn new(max_ops_per_second: u32) -> Self {
        Self { max_ops_per_second, window_start_ns: 0, ops_in_window: 0 }
    }

    /// Checks if an operation is permitted at `now_ns`.
    pub fn check_and_record(&mut self, now_ns: i64) -> Result<(), Denial> {
        if self.max_ops_per_second == 0 {
            return Ok(());
        }
        let window_duration_ns: i64 = 1_000_000_000;
        if now_ns - self.window_start_ns >= window_duration_ns {
            self.window_start_ns = now_ns;
            self.ops_in_window = 1;
            Ok(())
        } else if self.ops_in_window < self.max_ops_per_second {
            self.ops_in_window += 1;
            Ok(())
        } else {
            Err(Denial::LimitExceeded)
        }
    }
}

/// Enforces access-class count, rate, and deadline limits (Spec 12.1 step 9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessLimitEnforcer {
    global_rate_limiter: RateLimiter,
    per_class_rate_limiters: BTreeMap<AccessClass, RateLimiter>,
    max_deadline_ns: u64,
}

impl AccessLimitEnforcer {
    pub fn new(max_ops_per_second: u32, max_deadline_ns: u64) -> Self {
        Self {
            global_rate_limiter: RateLimiter::new(max_ops_per_second),
            per_class_rate_limiters: BTreeMap::new(),
            max_deadline_ns,
        }
    }

    pub fn set_class_rate_limit(&mut self, class: AccessClass, max_ops_per_second: u32) {
        self.per_class_rate_limiters.insert(class, RateLimiter::new(max_ops_per_second));
    }

    /// Enforces access-class rate limit at `now_ns`.
    pub fn check_access(&mut self, class: AccessClass, now_ns: i64) -> Result<(), Denial> {
        self.global_rate_limiter.check_and_record(now_ns)?;
        if let Some(limiter) = self.per_class_rate_limiters.get_mut(&class) {
            limiter.check_and_record(now_ns)?;
        }
        Ok(())
    }

    /// Enforces maximum deadline limit.
    pub fn check_deadline(&self, requested_timeout_ns: i64) -> Result<(), Denial> {
        if requested_timeout_ns < 0 || (requested_timeout_ns as u64) > self.max_deadline_ns {
            Err(Denial::LimitExceeded)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_config_defaults() {
        let config = ProxyConfig::default();
        assert!(config.enabled);
        assert_eq!(config.audit_capacity, BASELINE_AUDIT_CAPACITY);
        assert_eq!(config.max_snapshot_items, BASELINE_MAX_SNAPSHOT_ITEMS);
        assert_eq!(config.max_sequence_items, BASELINE_MAX_SEQUENCE_ITEMS);
        assert_eq!(config.max_delay_ns, BASELINE_MAX_DELAY_NS);
        assert_eq!(config.max_sequence_duration_ns, BASELINE_MAX_SEQUENCE_DURATION_NS);
        assert_eq!(config.max_deadline_ns, BASELINE_MAX_DEADLINE_NS);
        assert_eq!(config.max_ops_per_second, 0);
    }

    #[test]
    fn test_narrow_with_accepts_valid_reductions() {
        let baseline = ProxyConfig::default();
        let reduced = ProxyConfig {
            enabled: false,
            audit_capacity: 512,
            max_snapshot_items: 16,
            max_sequence_items: 16,
            max_delay_ns: 50_000_000,
            max_sequence_duration_ns: 500_000_000,
            max_deadline_ns: 500_000_000,
            max_ops_per_second: 100,
        };
        let narrowed = baseline.narrow_with(&reduced).unwrap();
        assert_eq!(narrowed, reduced);
    }

    #[test]
    fn test_narrow_with_rejects_widening() {
        let baseline = ProxyConfig { enabled: false, ..ProxyConfig::default() };
        assert_eq!(
            baseline.narrow_with(&ProxyConfig::default()),
            Err(ConfigNarrowingError::WidenedEnabledState)
        );

        let baseline = ProxyConfig::default();
        let mut widened = baseline;
        widened.audit_capacity = 2048;
        assert!(matches!(
            baseline.narrow_with(&widened),
            Err(ConfigNarrowingError::WidenedAuditCapacity { .. })
        ));

        let mut widened = baseline;
        widened.max_snapshot_items = 128;
        assert!(matches!(
            baseline.narrow_with(&widened),
            Err(ConfigNarrowingError::WidenedMaxSnapshotItems { .. })
        ));

        let mut widened = baseline;
        widened.max_sequence_items = 128;
        assert!(matches!(
            baseline.narrow_with(&widened),
            Err(ConfigNarrowingError::WidenedMaxSequenceItems { .. })
        ));

        let mut widened = baseline;
        widened.max_delay_ns = 2_000_000_000;
        assert!(matches!(
            baseline.narrow_with(&widened),
            Err(ConfigNarrowingError::WidenedMaxDelay { .. })
        ));

        let mut widened = baseline;
        widened.max_sequence_duration_ns = 2_000_000_000;
        assert!(matches!(
            baseline.narrow_with(&widened),
            Err(ConfigNarrowingError::WidenedMaxSequenceDuration { .. })
        ));

        let mut widened = baseline;
        widened.max_deadline_ns = 2_000_000_000;
        assert!(matches!(
            baseline.narrow_with(&widened),
            Err(ConfigNarrowingError::WidenedMaxDeadline { .. })
        ));

        let baseline = ProxyConfig { max_ops_per_second: 50, ..ProxyConfig::default() };
        let mut widened = baseline;
        widened.max_ops_per_second = 100;
        assert!(matches!(
            baseline.narrow_with(&widened),
            Err(ConfigNarrowingError::WidenedMaxOpsPerSecond { .. })
        ));
    }

    #[test]
    fn test_rate_limiter() {
        let mut limiter = RateLimiter::new(2);
        let t0 = 1_000_000_000;
        assert!(limiter.check_and_record(t0).is_ok());
        assert!(limiter.check_and_record(t0 + 100_000_000).is_ok());
        // 3rd operation in the same 1s window is rejected
        assert_eq!(limiter.check_and_record(t0 + 200_000_000), Err(Denial::LimitExceeded));

        // Next second window resets counter
        let t1 = t0 + 1_000_000_001;
        assert!(limiter.check_and_record(t1).is_ok());
    }

    #[test]
    fn test_access_limit_enforcer_and_deadline() {
        let mut enforcer = AccessLimitEnforcer::new(100, 1_000_000_000);
        enforcer.set_class_rate_limit(AccessClass::Poll, 1);

        assert!(enforcer.check_access(AccessClass::ReadOnce, 1_000_000).is_ok());
        assert!(enforcer.check_access(AccessClass::Poll, 1_000_000).is_ok());
        // Second poll in same second is rejected by class rate limit
        assert_eq!(enforcer.check_access(AccessClass::Poll, 1_500_000), Err(Denial::LimitExceeded));
        // But read_once is still allowed under global limit
        assert!(enforcer.check_access(AccessClass::ReadOnce, 1_500_000).is_ok());

        // Deadline check
        assert!(enforcer.check_deadline(500_000_000).is_ok());
        assert_eq!(enforcer.check_deadline(1_500_000_000), Err(Denial::LimitExceeded));
        assert_eq!(enforcer.check_deadline(-1), Err(Denial::LimitExceeded));
    }
}
