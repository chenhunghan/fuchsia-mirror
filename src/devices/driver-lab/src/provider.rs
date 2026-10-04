// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Provider-neutral bundle of acquired resources.
//!
//! A resource provider (for example the platform-device adapter in the
//! driver crate) discovers the resources offered to the bound node and
//! produces one [`ProvidedResources`]: descriptions, the immutable
//! ceiling entries that apply to them, and a backend per resource. The
//! bundle is validated before the proxy serves anything, so a provider
//! bug fails driver start instead of producing a resource without policy
//! or a policy without a resource.

use crate::access_policy::{
    MmioResource, ProtocolCeiling, ResourceCeiling, ResourceId, ResourceKind,
};
use crate::digest::{Sha256Digest, combined_digest, per_resource_digests};
use std::collections::BTreeMap;

/// Provider kind used by the embedded driver library (Phase 2).
pub const EMBEDDED_PROVIDER: &str = "embedded";

/// Everything one provider acquired for the bound node.
#[derive(Debug)]
pub struct ProvidedResources<B> {
    /// Provider kind, for example `"platform"`, `"embedded"`, or `"fake"`. Feeds the
    /// per-resource digests.
    pub provider: String,
    /// Stable node identity criteria (for example the node moniker),
    /// never a per-boot numeric ID. Feeds the per-resource digests.
    pub node_identity: String,
    /// Logical resource descriptions, keyed by resource ordinal.
    pub resources: BTreeMap<ResourceId, MmioResource>,
    /// Immutable ceiling entries. A resource with no entry permits
    /// nothing.
    pub ceiling: BTreeMap<ResourceId, ResourceCeiling>,
    /// One backend per offered resource.
    pub backends: BTreeMap<ResourceId, B>,
}

/// Why a provider bundle was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderError {
    /// A resource has no backend, or a backend has no resource.
    ResourceBackendMismatch {
        /// Resource ordinals present on exactly one side.
        missing: Vec<ResourceId>,
    },
    /// A ceiling entry names a resource that is not offered. The reverse
    /// (an offered resource without a ceiling entry) is legal and simply
    /// permits nothing.
    CeilingWithoutResource {
        /// The dangling ceiling ordinals.
        missing: Vec<ResourceId>,
    },
    /// A resource's mapped size is smaller than its logical size, so
    /// reads the description advertises could fault.
    MappingSmallerThanLogical {
        /// The offending resource ordinal.
        resource: ResourceId,
    },
}

impl<B> ProvidedResources<B> {
    /// An empty bundle: no resources, nothing reachable.
    pub fn empty(provider: &str, node_identity: &str) -> Self {
        Self {
            provider: provider.to_string(),
            node_identity: node_identity.to_string(),
            resources: BTreeMap::new(),
            ceiling: BTreeMap::new(),
            backends: BTreeMap::new(),
        }
    }

    /// Creates an empty bundle for an embedded in-situ driver library.
    pub fn embedded(node_identity: &str) -> Self {
        Self::empty(EMBEDDED_PROVIDER, node_identity)
    }

    /// Registers a pre-mapped MMIO bank with its logical size, actual mapped size,
    /// target ceiling policy, and backend.
    pub fn add_mmio(
        &mut self,
        id: ResourceId,
        name: impl Into<String>,
        logical_size: u64,
        mapped_size: u64,
        ceiling: ResourceCeiling,
        backend: B,
    ) {
        self.resources.insert(id, MmioResource::mmio(name, logical_size, mapped_size));
        self.ceiling.insert(id, ceiling);
        self.backends.insert(id, backend);
    }

    /// Registers a protocol-backed resource (GPIO, I2C, SPI, Clock, Reset, Serial).
    pub fn add_protocol(
        &mut self,
        id: ResourceId,
        name: impl Into<String>,
        kind: ResourceKind,
        ceiling: ProtocolCeiling,
        backend: B,
    ) {
        let resource = match kind {
            ResourceKind::Gpio => MmioResource::gpio(name),
            ResourceKind::I2c => MmioResource::i2c(name),
            ResourceKind::Spi => MmioResource::spi(name),
            ResourceKind::Clock => MmioResource::clock(name),
            ResourceKind::Reset => MmioResource::reset(name),
            ResourceKind::Serial => MmioResource::serial(name),
            ResourceKind::Interrupt => MmioResource::interrupt(name),
            ResourceKind::Mmio => MmioResource::mmio(name, 0, 0),
        };
        self.resources.insert(id, resource);
        self.ceiling.insert(
            id,
            ResourceCeiling {
                hard_denied: vec![],
                allow_unknown_reads: false,
                allow_poll: false,
                writable_registers: vec![],
                protocol: Some(ceiling),
                allow_interrupt: kind == ResourceKind::Interrupt,
            },
        );
        self.backends.insert(id, backend);
    }

    /// Registers an interrupt observation resource.
    pub fn add_interrupt(&mut self, id: ResourceId, name: impl Into<String>, backend: B) {
        self.resources.insert(id, MmioResource::interrupt(name));
        self.ceiling.insert(
            id,
            ResourceCeiling {
                hard_denied: vec![],
                allow_unknown_reads: false,
                allow_poll: false,
                writable_registers: vec![],
                protocol: Some(ProtocolCeiling::default_for(ResourceKind::Interrupt)),
                allow_interrupt: true,
            },
        );
        self.backends.insert(id, backend);
    }

    /// Validates internal consistency. Driver start fails on error.
    pub fn validate(&self) -> Result<(), ProviderError> {
        let resource_ids: Vec<ResourceId> = self.resources.keys().copied().collect();
        let backend_ids: Vec<ResourceId> = self.backends.keys().copied().collect();
        if resource_ids != backend_ids {
            let missing = resource_ids
                .iter()
                .filter(|id| !self.backends.contains_key(id))
                .chain(backend_ids.iter().filter(|id| !self.resources.contains_key(id)))
                .copied()
                .collect();
            return Err(ProviderError::ResourceBackendMismatch { missing });
        }
        let dangling: Vec<ResourceId> =
            self.ceiling.keys().filter(|id| !self.resources.contains_key(id)).copied().collect();
        if !dangling.is_empty() {
            return Err(ProviderError::CeilingWithoutResource { missing: dangling });
        }
        for (id, resource) in &self.resources {
            if resource.mapped_size < resource.logical_size {
                return Err(ProviderError::MappingSmallerThanLogical { resource: *id });
            }
        }
        Ok(())
    }

    /// Per-resource digests for this bundle, in ascending resource ID
    /// order.
    pub fn digests(&self) -> BTreeMap<ResourceId, Sha256Digest> {
        per_resource_digests(&self.node_identity, &self.provider, &self.resources, &self.ceiling)
    }

    /// The whole-instance resource digest for this bundle.
    pub fn combined_digest(&self) -> Sha256Digest {
        combined_digest(self.digests().values().copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware_backend::FakeMmio;

    fn resource(logical: u64, mapped: u64) -> MmioResource {
        MmioResource {
            name: "ctrl".to_string(),
            kind: crate::access_policy::ResourceKind::Mmio,
            logical_size: logical,
            mapped_size: mapped,
        }
    }

    fn bundle() -> ProvidedResources<FakeMmio> {
        let mut bundle = ProvidedResources::empty("fake", "node");
        bundle.resources.insert(1, resource(0x100, 0x1000));
        bundle.ceiling.insert(
            1,
            ResourceCeiling {
                hard_denied: vec![],
                allow_unknown_reads: true,
                allow_poll: false,
                writable_registers: vec![],
                protocol: None,
                allow_interrupt: false,
            },
        );
        bundle.backends.insert(1, FakeMmio::new());
        bundle
    }

    #[test]
    fn valid_bundle_passes() {
        assert_eq!(bundle().validate(), Ok(()));
        assert_eq!(ProvidedResources::<FakeMmio>::empty("fake", "node").validate(), Ok(()));
    }

    #[test]
    fn missing_backend_is_rejected() {
        let mut bundle = bundle();
        bundle.backends.clear();
        assert_eq!(
            bundle.validate(),
            Err(ProviderError::ResourceBackendMismatch { missing: vec![1] })
        );
    }

    #[test]
    fn dangling_ceiling_is_rejected() {
        let mut bundle = bundle();
        bundle.ceiling.insert(
            9,
            ResourceCeiling {
                hard_denied: vec![],
                allow_unknown_reads: false,
                allow_poll: false,
                writable_registers: vec![],
                protocol: None,
                allow_interrupt: false,
            },
        );
        assert_eq!(
            bundle.validate(),
            Err(ProviderError::CeilingWithoutResource { missing: vec![9] })
        );
    }

    #[test]
    fn resource_without_ceiling_is_legal() {
        let mut bundle = bundle();
        bundle.ceiling.clear();
        assert_eq!(bundle.validate(), Ok(()));
    }

    #[test]
    fn short_mapping_is_rejected() {
        let mut bundle = bundle();
        bundle.resources.insert(1, resource(0x1000, 0x100));
        assert_eq!(
            bundle.validate(),
            Err(ProviderError::MappingSmallerThanLogical { resource: 1 })
        );
    }

    #[test]
    fn digests_are_ordered_and_combined() {
        let bundle = bundle();
        let digests = bundle.digests();
        assert_eq!(digests.len(), 1);
        assert_eq!(bundle.combined_digest(), combined_digest(digests.values().copied()));
    }

    #[test]
    fn embedded_provider_registers_mmio_protocol_and_interrupt() {
        let mut bundle = ProvidedResources::embedded("sample-driver");
        assert_eq!(bundle.provider, EMBEDDED_PROVIDER);
        assert_eq!(bundle.node_identity, "sample-driver");

        bundle.add_mmio(
            0,
            "regs",
            0x100,
            0x1000,
            ResourceCeiling {
                hard_denied: vec![0x40..0x44],
                allow_unknown_reads: true,
                allow_poll: true,
                writable_registers: vec![],
                protocol: None,
                allow_interrupt: false,
            },
            FakeMmio::new(),
        );
        bundle.add_protocol(
            1,
            "gpio0",
            ResourceKind::Gpio,
            ProtocolCeiling::default_for(ResourceKind::Gpio),
            FakeMmio::new(),
        );
        bundle.add_interrupt(2, "irq0", FakeMmio::new());

        assert_eq!(bundle.validate(), Ok(()));
        assert_eq!(bundle.resources.len(), 3);
        assert_eq!(bundle.resources[&0].kind, ResourceKind::Mmio);
        assert_eq!(bundle.resources[&1].kind, ResourceKind::Gpio);
        assert_eq!(bundle.resources[&2].kind, ResourceKind::Interrupt);
        assert!(bundle.ceiling[&2].allow_interrupt);
    }
}
