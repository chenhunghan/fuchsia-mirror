// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Target ceiling policy manifest representation, canonicalization, and
//! narrowing verification.
//!
//! The target ceiling defines the immutable hardware boundaries and safety
//! constraints for the proxy driver. Runtime configuration (such as structured
//! configuration) may only narrow this ceiling, never widen it (Spec 9.2, 22).
//!
//! Manifests are canonicalized and digested via SHA-256 (Spec 9.6).

use crate::access_policy::{
    MmioResource, ResourceCeiling, ResourceId, ResourceKind, WritableRegister,
};
use crate::digest::Sha256Digest;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::ops::Range;

/// The schema version for target ceiling manifests.
pub const TARGET_POLICY_SCHEMA_VERSION: u32 = 1;

/// Maximum resources allowed by the wire contract.
pub const MAX_RESOURCES: usize = 64;

/// Maximum snapshot items allowed by the wire contract.
pub const MAX_SNAPSHOT_ITEMS: u32 = 64;

/// Maximum name length allowed for a resource.
pub const MAX_NAME_LENGTH: usize = 64;

/// A byte range `[start, end)` representing a hard-denied region.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RangeDto {
    pub start: u64,
    pub end: u64,
}

impl RangeDto {
    /// Creates and validates a range.
    pub fn new(start: u64, end: u64) -> Result<Self, PolicyValidationError> {
        if start >= end {
            return Err(PolicyValidationError::InvalidRange { start, end });
        }
        Ok(Self { start, end })
    }

    /// Converts to standard Rust range.
    pub fn to_range(&self) -> Range<u64> {
        self.start..self.end
    }

    /// Returns true if this range contains `offset`.
    pub fn contains_offset(&self, offset: u64) -> bool {
        self.start <= offset && offset < self.end
    }
}

impl Serialize for RangeDto {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        (self.start, self.end).serialize(serializer)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RangeHelper {
    Seq((u64, u64)),
    Map { start: u64, end: u64 },
}

impl<'de> Deserialize<'de> for RangeDto {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let helper = RangeHelper::deserialize(deserializer)?;
        let (start, end) = match helper {
            RangeHelper::Seq((s, e)) => (s, e),
            RangeHelper::Map { start, end } => (start, end),
        };
        RangeDto::new(start, end).map_err(serde::de::Error::custom)
    }
}

/// Manifest specification for one writable register.
///
/// Fields are declared in alphabetical order to produce deterministic canonical JSON.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WritableRegisterDto {
    /// Allowed writable bitmask.
    pub allow_mask: u32,
    /// Whether read-modify-write is allowed.
    pub allow_rmw: bool,
    /// Byte offset from the start of the logical resource.
    pub offset: u64,
    /// Valid precondition bitmask.
    pub precondition_mask: u32,
    /// Whether readback is configured/supported.
    pub readback: bool,
    /// Whether a precondition is required for writes.
    pub require_precondition: bool,
    /// Access width in bytes (must be 4).
    pub width: u32,
}

impl WritableRegisterDto {
    pub fn to_writable_register(&self) -> WritableRegister {
        WritableRegister {
            offset: self.offset,
            width: self.width,
            allow_mask: self.allow_mask,
            allow_rmw: self.allow_rmw,
            require_precondition: self.require_precondition,
            precondition_mask: self.precondition_mask,
            readback: self.readback,
        }
    }
}

/// Manifest specification for one resource's ceiling.
///
/// Fields are declared in alphabetical order to produce deterministic canonical JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePolicyManifest {
    /// Whether interrupt observation may be authorized by session allowlist.
    #[serde(default)]
    pub allow_interrupt: bool,
    /// Whether polling reads may be authorized by session allowlist.
    pub allow_poll: bool,
    /// Whether operator-authorized unknown reads may be enabled.
    pub allow_unknown_reads: bool,
    /// Byte ranges that may never be read, even with operator consent.
    pub hard_denied: Vec<RangeDto>,
    /// Resource ID.
    pub id: ResourceId,
    /// Stable logical resource name.
    pub name: String,
    /// Writable registers permitted on this resource.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable_registers: Vec<WritableRegisterDto>,
}

impl ResourcePolicyManifest {
    /// Validates and canonicalizes resource policy fields.
    pub fn canonicalize(&mut self) -> Result<(), PolicyValidationError> {
        if self.name.is_empty() {
            return Err(PolicyValidationError::EmptyResourceName(self.id));
        }
        if self.name.len() > MAX_NAME_LENGTH {
            return Err(PolicyValidationError::ResourceNameTooLong {
                id: self.id,
                len: self.name.len(),
                max: MAX_NAME_LENGTH,
            });
        }
        self.hard_denied = canonicalize_ranges(&self.hard_denied)?;
        self.writable_registers.sort();
        let mut seen = std::collections::BTreeSet::new();
        for reg in &self.writable_registers {
            if !seen.insert(reg.offset) {
                return Err(PolicyValidationError::DuplicateWritableRegister {
                    id: self.id,
                    offset: reg.offset,
                });
            }
            if reg.width != 4 {
                return Err(PolicyValidationError::UnsupportedWidth(reg.width));
            }
            if reg.allow_mask == 0 {
                return Err(PolicyValidationError::InvalidWritableRegisterMask {
                    id: self.id,
                    offset: reg.offset,
                });
            }
        }
        Ok(())
    }

    /// Converts this manifest entry to `ResourceCeiling`.
    pub fn to_ceiling(&self) -> ResourceCeiling {
        ResourceCeiling {
            hard_denied: self.hard_denied.iter().map(|r| r.to_range()).collect(),
            allow_unknown_reads: self.allow_unknown_reads,
            allow_poll: self.allow_poll,
            writable_registers: self
                .writable_registers
                .iter()
                .map(|w| w.to_writable_register())
                .collect(),
            protocol: None,
            allow_interrupt: self.allow_interrupt,
        }
    }
}

/// Target-wide ceiling policy manifest.
///
/// Fields are declared in alphabetical order to produce deterministic canonical JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetPolicyManifest {
    /// Whether mutating sessions may be opened.
    pub allow_mutating_sessions: bool,
    /// Bounded audit capacity.
    pub audit_capacity: u64,
    /// Maximum snapshot items per bounded read snapshot.
    pub max_snapshot_items: u32,
    /// Per-resource ceiling policies, sorted by resource ID.
    pub resources: Vec<ResourcePolicyManifest>,
    /// Schema version (must be 1).
    pub schema_version: u32,
}

impl TargetPolicyManifest {
    /// Default engineering ceiling policy for Phase 1 exploration.
    pub fn engineering_default(resources: &BTreeMap<ResourceId, MmioResource>) -> Self {
        let resource_manifests = resources
            .iter()
            .map(|(id, res)| ResourcePolicyManifest {
                id: *id,
                name: res.name.clone(),
                allow_interrupt: res.kind == ResourceKind::Interrupt,
                allow_unknown_reads: true,
                allow_poll: false,
                hard_denied: vec![],
                writable_registers: vec![],
            })
            .collect();

        Self {
            schema_version: TARGET_POLICY_SCHEMA_VERSION,
            allow_mutating_sessions: true,
            max_snapshot_items: MAX_SNAPSHOT_ITEMS,
            audit_capacity: 1024,
            resources: resource_manifests,
        }
    }

    /// Constructs a canonical target policy manifest from offered resources and
    /// their configured per-resource ceiling entries (including `hard_denied`
    /// ranges and `writable_registers`).
    pub fn from_bundle_ceiling(
        resources: &BTreeMap<ResourceId, MmioResource>,
        ceiling: &BTreeMap<ResourceId, ResourceCeiling>,
        allow_mutating_sessions: bool,
    ) -> Self {
        let resource_manifests = resources
            .iter()
            .map(|(id, res)| {
                if let Some(c) = ceiling.get(id) {
                    ResourcePolicyManifest {
                        id: *id,
                        name: res.name.clone(),
                        allow_interrupt: c.allow_interrupt,
                        allow_unknown_reads: c.allow_unknown_reads,
                        allow_poll: c.allow_poll,
                        hard_denied: c
                            .hard_denied
                            .iter()
                            .filter_map(|r| RangeDto::new(r.start, r.end).ok())
                            .collect(),
                        writable_registers: c
                            .writable_registers
                            .iter()
                            .map(|w| WritableRegisterDto {
                                allow_mask: w.allow_mask,
                                allow_rmw: w.allow_rmw,
                                offset: w.offset,
                                precondition_mask: w.precondition_mask,
                                readback: w.readback,
                                require_precondition: w.require_precondition,
                                width: w.width,
                            })
                            .collect(),
                    }
                } else {
                    ResourcePolicyManifest {
                        id: *id,
                        name: res.name.clone(),
                        allow_interrupt: res.kind == ResourceKind::Interrupt,
                        allow_unknown_reads: false,
                        allow_poll: false,
                        hard_denied: vec![],
                        writable_registers: vec![],
                    }
                }
            })
            .collect();

        let mut manifest = Self {
            schema_version: TARGET_POLICY_SCHEMA_VERSION,
            allow_mutating_sessions,
            max_snapshot_items: MAX_SNAPSHOT_ITEMS,
            audit_capacity: 1024,
            resources: resource_manifests,
        };
        let _ = manifest.canonicalize();
        manifest
    }

    /// Validates and canonicalizes the manifest.
    pub fn canonicalize(&mut self) -> Result<(), PolicyValidationError> {
        if self.schema_version != TARGET_POLICY_SCHEMA_VERSION {
            return Err(PolicyValidationError::InvalidSchemaVersion(self.schema_version));
        }
        if self.max_snapshot_items == 0 {
            return Err(PolicyValidationError::ZeroSnapshotLimit);
        }
        if self.max_snapshot_items > MAX_SNAPSHOT_ITEMS {
            return Err(PolicyValidationError::SnapshotLimitExceeded {
                value: self.max_snapshot_items,
                max: MAX_SNAPSHOT_ITEMS,
            });
        }
        if self.audit_capacity == 0 {
            return Err(PolicyValidationError::ZeroAuditCapacity);
        }
        if self.resources.len() > MAX_RESOURCES {
            return Err(PolicyValidationError::TooManyResources {
                count: self.resources.len(),
                max: MAX_RESOURCES,
            });
        }

        self.resources.sort_by_key(|r| r.id);
        let mut seen_ids = std::collections::BTreeSet::new();
        let mut seen_names = std::collections::BTreeSet::new();

        for res in &mut self.resources {
            if !seen_ids.insert(res.id) {
                return Err(PolicyValidationError::DuplicateResourceId(res.id));
            }
            if !seen_names.insert(res.name.clone()) {
                return Err(PolicyValidationError::DuplicateResourceName(res.name.clone()));
            }
            res.canonicalize()?;
        }

        Ok(())
    }

    /// Serializes to canonical JSON.
    pub fn to_canonical_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Computes the canonical SHA-256 digest of the manifest.
    pub fn policy_digest(&self) -> Sha256Digest {
        let json_str = self.to_canonical_json().expect("manifest serializes to JSON");
        let hash = Sha256::digest(json_str.as_bytes());
        Sha256Digest::from_bytes(hash.into())
    }

    /// Converts resource manifests to a ceiling map for the policy engine.
    pub fn to_ceiling_map(&self) -> BTreeMap<ResourceId, ResourceCeiling> {
        self.resources.iter().map(|r| (r.id, r.to_ceiling())).collect()
    }

    /// Validates that `runtime` narrows or preserves (does not widen) this baseline manifest.
    /// Returns the validated narrowed manifest.
    pub fn narrow_with(&self, runtime: &Self) -> Result<Self, NarrowingError> {
        if self.schema_version != runtime.schema_version {
            return Err(NarrowingError::SchemaVersionMismatch);
        }

        // Cannot enable mutating sessions if baseline disallows.
        if !self.allow_mutating_sessions && runtime.allow_mutating_sessions {
            return Err(NarrowingError::WidenedMutatingSessions);
        }

        // Snapshot limit can only decrease or stay the same.
        if runtime.max_snapshot_items > self.max_snapshot_items {
            return Err(NarrowingError::WidenedSnapshotLimit {
                baseline: self.max_snapshot_items,
                runtime: runtime.max_snapshot_items,
            });
        }

        // Audit capacity can only decrease or stay the same.
        if runtime.audit_capacity > self.audit_capacity {
            return Err(NarrowingError::WidenedAuditCapacity {
                baseline: self.audit_capacity,
                runtime: runtime.audit_capacity,
            });
        }

        let base_resources: BTreeMap<ResourceId, &ResourcePolicyManifest> =
            self.resources.iter().map(|r| (r.id, r)).collect();

        for runtime_res in &runtime.resources {
            let base_res = base_resources
                .get(&runtime_res.id)
                .ok_or(NarrowingError::UnknownResource(runtime_res.id))?;

            if runtime_res.name != base_res.name {
                return Err(NarrowingError::ResourceNameMismatch {
                    id: runtime_res.id,
                    baseline: base_res.name.clone(),
                    runtime: runtime_res.name.clone(),
                });
            }

            // Cannot enable unknown reads if baseline disallows.
            if !base_res.allow_unknown_reads && runtime_res.allow_unknown_reads {
                return Err(NarrowingError::WidenedUnknownReads(runtime_res.id));
            }

            // Cannot enable polling if baseline disallows.
            if !base_res.allow_poll && runtime_res.allow_poll {
                return Err(NarrowingError::WidenedPoll(runtime_res.id));
            }

            // Cannot enable interrupt observation if baseline disallows.
            if !base_res.allow_interrupt && runtime_res.allow_interrupt {
                return Err(NarrowingError::WidenedInterrupt(runtime_res.id));
            }

            // Runtime MUST preserve all hard-denied bytes from baseline.
            // Every base range must be fully covered by a runtime range.
            for base_range in &base_res.hard_denied {
                let covered = runtime_res.hard_denied.iter().any(|rt_range| {
                    rt_range.start <= base_range.start && base_range.end <= rt_range.end
                });
                if !covered {
                    return Err(NarrowingError::ReducedHardDenial {
                        id: runtime_res.id,
                        unconstrained_start: base_range.start,
                        unconstrained_end: base_range.end,
                    });
                }
            }

            // Runtime writable registers must exist in baseline and cannot widen masks or permissions.
            for rt_reg in &runtime_res.writable_registers {
                let base_reg =
                    base_res.writable_registers.iter().find(|b| b.offset == rt_reg.offset).ok_or(
                        NarrowingError::WidenedWritableRegister {
                            id: runtime_res.id,
                            offset: rt_reg.offset,
                        },
                    )?;
                if (rt_reg.allow_mask & !base_reg.allow_mask) != 0 {
                    return Err(NarrowingError::WidenedWritableRegisterMask {
                        id: runtime_res.id,
                        offset: rt_reg.offset,
                    });
                }
                if !base_reg.allow_rmw && rt_reg.allow_rmw {
                    return Err(NarrowingError::WidenedWritableRegister {
                        id: runtime_res.id,
                        offset: rt_reg.offset,
                    });
                }
                if base_reg.require_precondition && !rt_reg.require_precondition {
                    return Err(NarrowingError::WidenedWritableRegister {
                        id: runtime_res.id,
                        offset: rt_reg.offset,
                    });
                }
                if (rt_reg.precondition_mask & !base_reg.precondition_mask) != 0 {
                    return Err(NarrowingError::WidenedWritableRegisterMask {
                        id: runtime_res.id,
                        offset: rt_reg.offset,
                    });
                }
            }
        }

        let mut narrowed = runtime.clone();
        narrowed.canonicalize().map_err(NarrowingError::Validation)?;
        Ok(narrowed)
    }
}

/// Merges overlapping and adjacent ranges and validates `start < end`.
pub fn canonicalize_ranges(ranges: &[RangeDto]) -> Result<Vec<RangeDto>, PolicyValidationError> {
    if ranges.is_empty() {
        return Ok(Vec::new());
    }

    let mut sorted = ranges.to_vec();
    for r in &sorted {
        if r.start >= r.end {
            return Err(PolicyValidationError::InvalidRange { start: r.start, end: r.end });
        }
    }
    sorted.sort_unstable_by_key(|r| (r.start, r.end));

    let mut merged: Vec<RangeDto> = Vec::with_capacity(sorted.len());
    for r in sorted {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => {
                // Overlapping or adjacent: merge
                last.end = std::cmp::max(last.end, r.end);
            }
            _ => merged.push(r),
        }
    }

    Ok(merged)
}

/// Errors during manifest validation or canonicalization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyValidationError {
    InvalidSchemaVersion(u32),
    InvalidRange { start: u64, end: u64 },
    SnapshotLimitExceeded { value: u32, max: u32 },
    ZeroSnapshotLimit,
    ZeroAuditCapacity,
    EmptyResourceName(ResourceId),
    ResourceNameTooLong { id: ResourceId, len: usize, max: usize },
    DuplicateResourceId(ResourceId),
    DuplicateResourceName(String),
    TooManyResources { count: usize, max: usize },
    DuplicateWritableRegister { id: ResourceId, offset: u64 },
    UnsupportedWidth(u32),
    InvalidWritableRegisterMask { id: ResourceId, offset: u64 },
}

/// Errors when a runtime configuration widens rather than narrows the baseline policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NarrowingError {
    SchemaVersionMismatch,
    WidenedMutatingSessions,
    WidenedSnapshotLimit { baseline: u32, runtime: u32 },
    WidenedAuditCapacity { baseline: u64, runtime: u64 },
    UnknownResource(ResourceId),
    ResourceNameMismatch { id: ResourceId, baseline: String, runtime: String },
    WidenedUnknownReads(ResourceId),
    WidenedPoll(ResourceId),
    WidenedInterrupt(ResourceId),
    ReducedHardDenial { id: ResourceId, unconstrained_start: u64, unconstrained_end: u64 },
    WidenedWritableRegister { id: ResourceId, offset: u64 },
    WidenedWritableRegisterMask { id: ResourceId, offset: u64 },
    Validation(PolicyValidationError),
}

impl std::fmt::Display for PolicyValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSchemaVersion(v) => write!(f, "invalid schema version: {v}"),
            Self::InvalidRange { start, end } => {
                write!(f, "invalid range: start {start} >= end {end}")
            }
            Self::SnapshotLimitExceeded { value, max } => {
                write!(f, "snapshot limit {value} exceeds maximum {max}")
            }
            Self::ZeroSnapshotLimit => write!(f, "snapshot limit must be greater than zero"),
            Self::ZeroAuditCapacity => write!(f, "audit capacity must be greater than zero"),
            Self::EmptyResourceName(id) => write!(f, "empty resource name for resource {id}"),
            Self::ResourceNameTooLong { id, len, max } => {
                write!(f, "resource name for {id} length {len} exceeds maximum {max}")
            }
            Self::DuplicateResourceId(id) => write!(f, "duplicate resource ID: {id}"),
            Self::DuplicateResourceName(name) => write!(f, "duplicate resource name: {name}"),
            Self::TooManyResources { count, max } => {
                write!(f, "resource count {count} exceeds maximum {max}")
            }
            Self::DuplicateWritableRegister { id, offset } => {
                write!(f, "duplicate writable register for resource {id} at offset {offset}")
            }
            Self::UnsupportedWidth(w) => write!(f, "unsupported width: {w}"),
            Self::InvalidWritableRegisterMask { id, offset } => {
                write!(f, "invalid allow mask for resource {id} at offset {offset}")
            }
        }
    }
}

impl std::error::Error for PolicyValidationError {}

impl std::fmt::Display for NarrowingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SchemaVersionMismatch => write!(f, "schema version mismatch"),
            Self::WidenedMutatingSessions => write!(f, "cannot enable mutating sessions"),
            Self::WidenedSnapshotLimit { baseline, runtime } => {
                write!(f, "snapshot limit {runtime} exceeds baseline {baseline}")
            }
            Self::WidenedAuditCapacity { baseline, runtime } => {
                write!(f, "audit capacity {runtime} exceeds baseline {baseline}")
            }
            Self::UnknownResource(id) => write!(f, "unknown resource {id} in runtime policy"),
            Self::ResourceNameMismatch { id, baseline, runtime } => {
                write!(f, "resource {id} name mismatch: baseline {baseline}, runtime {runtime}")
            }
            Self::WidenedUnknownReads(id) => {
                write!(f, "cannot enable unknown reads on resource {id}")
            }
            Self::WidenedPoll(id) => write!(f, "cannot enable polling on resource {id}"),
            Self::WidenedInterrupt(id) => {
                write!(f, "cannot enable interrupt observation on resource {id}")
            }
            Self::ReducedHardDenial { id, unconstrained_start, unconstrained_end } => {
                write!(
                    f,
                    "runtime hard denials for resource {id} fail to cover baseline denial [{unconstrained_start}, {unconstrained_end})"
                )
            }
            Self::WidenedWritableRegister { id, offset } => {
                write!(f, "writable register on resource {id} at offset {offset} widened baseline")
            }
            Self::WidenedWritableRegisterMask { id, offset } => {
                write!(
                    f,
                    "mask for writable register on resource {id} at offset {offset} widened baseline"
                )
            }
            Self::Validation(err) => write!(f, "validation error: {err}"),
        }
    }
}

impl std::error::Error for NarrowingError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_canonicalization_merges_overlapping_and_adjacent() {
        let input = vec![
            RangeDto::new(10, 20).unwrap(),
            RangeDto::new(15, 25).unwrap(),
            RangeDto::new(25, 30).unwrap(),
            RangeDto::new(40, 50).unwrap(),
        ];
        let merged = canonicalize_ranges(&input).unwrap();
        assert_eq!(merged, vec![RangeDto::new(10, 30).unwrap(), RangeDto::new(40, 50).unwrap(),]);
    }

    #[test]
    fn range_canonicalization_rejects_inverted_or_empty() {
        assert_eq!(
            RangeDto::new(20, 10),
            Err(PolicyValidationError::InvalidRange { start: 20, end: 10 })
        );
        assert_eq!(
            RangeDto::new(10, 10),
            Err(PolicyValidationError::InvalidRange { start: 10, end: 10 })
        );
    }

    #[test]
    fn canonical_json_and_digest_determinism() {
        let mut manifest = TargetPolicyManifest {
            schema_version: 1,
            allow_mutating_sessions: false,
            audit_capacity: 1024,
            max_snapshot_items: 64,
            resources: vec![ResourcePolicyManifest {
                id: 0,
                name: "mmio0".to_string(),
                allow_interrupt: false,
                allow_unknown_reads: true,
                allow_poll: false,
                hard_denied: vec![RangeDto::new(64, 68).unwrap()],
                writable_registers: vec![],
            }],
        };
        manifest.canonicalize().unwrap();

        let json_str = manifest.to_canonical_json().unwrap();
        assert_eq!(
            json_str,
            "{\"allow_mutating_sessions\":false,\"audit_capacity\":1024,\"max_snapshot_items\":64,\"resources\":[{\"allow_interrupt\":false,\"allow_poll\":false,\"allow_unknown_reads\":true,\"hard_denied\":[[64,68]],\"id\":0,\"name\":\"mmio0\"}],\"schema_version\":1}"
        );

        let digest = manifest.policy_digest();
        assert_eq!(
            digest.to_string(),
            "sha256:5511f2b70b6fcd8ed519f7e57d31e0becb17083e7f0004d48abfd7ab88034b4d"
        );
    }

    #[test]
    fn narrowing_accepts_valid_reductions() {
        let baseline = TargetPolicyManifest {
            schema_version: 1,
            allow_mutating_sessions: true,
            audit_capacity: 1024,
            max_snapshot_items: 64,
            resources: vec![ResourcePolicyManifest {
                id: 0,
                name: "mmio0".to_string(),
                allow_interrupt: true,
                allow_unknown_reads: true,
                allow_poll: true,
                hard_denied: vec![RangeDto::new(64, 68).unwrap()],
                writable_registers: vec![],
            }],
        };

        // Runtime narrows limits and adds a hard denial.
        let runtime = TargetPolicyManifest {
            schema_version: 1,
            allow_mutating_sessions: false,
            audit_capacity: 512,
            max_snapshot_items: 32,
            resources: vec![ResourcePolicyManifest {
                id: 0,
                name: "mmio0".to_string(),
                allow_interrupt: false,
                allow_unknown_reads: false,
                allow_poll: false,
                hard_denied: vec![RangeDto::new(64, 68).unwrap(), RangeDto::new(128, 144).unwrap()],
                writable_registers: vec![],
            }],
        };

        let narrowed = baseline.narrow_with(&runtime).unwrap();
        assert_eq!(narrowed.audit_capacity, 512);
        assert_eq!(narrowed.max_snapshot_items, 32);
        assert!(!narrowed.allow_mutating_sessions);
        assert!(!narrowed.resources[0].allow_unknown_reads);
        assert!(!narrowed.resources[0].allow_poll);
        assert!(!narrowed.resources[0].allow_interrupt);
        assert_eq!(narrowed.resources[0].hard_denied.len(), 2);
    }

    #[test]
    fn narrowing_rejects_widenings() {
        let baseline = TargetPolicyManifest {
            schema_version: 1,
            allow_mutating_sessions: false,
            audit_capacity: 512,
            max_snapshot_items: 32,
            resources: vec![ResourcePolicyManifest {
                id: 0,
                name: "mmio0".to_string(),
                allow_interrupt: false,
                allow_unknown_reads: false,
                allow_poll: false,
                hard_denied: vec![RangeDto::new(64, 128).unwrap()],
                writable_registers: vec![],
            }],
        };

        // 1. Enabling mutating sessions
        let mut widened = baseline.clone();
        widened.allow_mutating_sessions = true;
        assert_eq!(baseline.narrow_with(&widened), Err(NarrowingError::WidenedMutatingSessions));

        // 2. Increasing audit capacity
        let mut widened = baseline.clone();
        widened.audit_capacity = 1024;
        assert_eq!(
            baseline.narrow_with(&widened),
            Err(NarrowingError::WidenedAuditCapacity { baseline: 512, runtime: 1024 })
        );

        // 3. Increasing max snapshot items
        let mut widened = baseline.clone();
        widened.max_snapshot_items = 64;
        assert_eq!(
            baseline.narrow_with(&widened),
            Err(NarrowingError::WidenedSnapshotLimit { baseline: 32, runtime: 64 })
        );

        // 4. Enabling unknown reads
        let mut widened = baseline.clone();
        widened.resources[0].allow_unknown_reads = true;
        assert_eq!(baseline.narrow_with(&widened), Err(NarrowingError::WidenedUnknownReads(0)));

        // 5. Enabling poll
        let mut widened = baseline.clone();
        widened.resources[0].allow_poll = true;
        assert_eq!(baseline.narrow_with(&widened), Err(NarrowingError::WidenedPoll(0)));

        // 6. Enabling interrupt
        let mut widened = baseline.clone();
        widened.resources[0].allow_interrupt = true;
        assert_eq!(baseline.narrow_with(&widened), Err(NarrowingError::WidenedInterrupt(0)));

        // 7. Reducing hard denial (shrinking [64, 128) to [64, 100))
        let mut widened = baseline.clone();
        widened.resources[0].hard_denied = vec![RangeDto::new(64, 100).unwrap()];
        assert_eq!(
            baseline.narrow_with(&widened),
            Err(NarrowingError::ReducedHardDenial {
                id: 0,
                unconstrained_start: 64,
                unconstrained_end: 128
            })
        );
    }

    #[test]
    fn writable_registers_canonicalization_and_narrowing() {
        let base_reg = WritableRegisterDto {
            allow_mask: 0x0000FFFF,
            allow_rmw: false,
            offset: 0x10,
            precondition_mask: 0x00000001,
            readback: true,
            require_precondition: true,
            width: 4,
        };
        let mut baseline = TargetPolicyManifest {
            schema_version: 1,
            allow_mutating_sessions: true,
            audit_capacity: 1024,
            max_snapshot_items: 64,
            resources: vec![ResourcePolicyManifest {
                id: 0,
                name: "mmio0".to_string(),
                allow_interrupt: false,
                allow_unknown_reads: true,
                allow_poll: false,
                hard_denied: vec![],
                writable_registers: vec![base_reg.clone()],
            }],
        };
        baseline.canonicalize().unwrap();

        // Runtime cannot add a new writable register not in baseline
        let mut runtime = baseline.clone();
        runtime.resources[0].writable_registers.push(WritableRegisterDto {
            allow_mask: 0xFFFFFFFF,
            allow_rmw: true,
            offset: 0x20,
            precondition_mask: 0,
            readback: false,
            require_precondition: false,
            width: 4,
        });
        assert_eq!(
            baseline.narrow_with(&runtime),
            Err(NarrowingError::WidenedWritableRegister { id: 0, offset: 0x20 })
        );

        // Runtime cannot widen allow_mask
        let mut runtime = baseline.clone();
        runtime.resources[0].writable_registers[0].allow_mask = 0x0001FFFF;
        assert_eq!(
            baseline.narrow_with(&runtime),
            Err(NarrowingError::WidenedWritableRegisterMask { id: 0, offset: 0x10 })
        );

        // Runtime cannot enable RMW when baseline disallows
        let mut runtime = baseline.clone();
        runtime.resources[0].writable_registers[0].allow_rmw = true;
        assert_eq!(
            baseline.narrow_with(&runtime),
            Err(NarrowingError::WidenedWritableRegister { id: 0, offset: 0x10 })
        );

        // Runtime can narrow allow_mask
        let mut runtime = baseline.clone();
        runtime.resources[0].writable_registers[0].allow_mask = 0x000000FF;
        assert!(baseline.narrow_with(&runtime).is_ok());

        // Runtime can omit writable registers (narrowing to read-only)
        let mut runtime = baseline.clone();
        runtime.resources[0].writable_registers.clear();
        assert!(baseline.narrow_with(&runtime).is_ok());
    }
}
