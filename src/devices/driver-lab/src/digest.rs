// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Resource-description digests.
//!
//! A digest covers the stable identity of one offered resource plus the
//! immutable target-ceiling entries that apply to that resource, so a
//! persistent host read grant stops matching when resource layout or policy
//! changes, while a policy change on one resource does not invalidate
//! grants on unrelated resources. Dynamic values (mapping addresses, boot
//! IDs, handles, and the actual mapped size) are excluded.

use crate::access_policy::{MmioResource, ResourceCeiling, ResourceId, ResourceKind, WIDTH32};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::fmt;

/// A SHA-256 digest, displayed as `sha256:<lowercase hex>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sha256Digest([u8; 32]);
impl Sha256Digest {
    /// Creates a digest from raw 32 bytes.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Stable inputs identifying one resource for grant matching.
#[derive(Clone, Copy, Debug)]
pub struct DigestInputs<'a> {
    /// Stable node identity criteria (for example the node moniker), never
    /// a per-boot numeric ID.
    pub node_identity: &'a str,
    /// Provider kind, for example `"platform"`, `"pci"`, or `"fake"`.
    pub provider: &'a str,
    /// Resource ordinal.
    pub id: ResourceId,
    /// The resource description. The actual mapped size is deliberately
    /// excluded: it is a property of the current mapping, not of the
    /// logical resource.
    pub resource: &'a MmioResource,
    /// The ceiling entry that applies to this resource, or `None` when the
    /// ceiling has no entry (the resource permits nothing).
    pub ceiling: Option<&'a ResourceCeiling>,
}

const RESOURCE_DOMAIN: &str = "fuchsia.driver.lab resource digest v1";
const COMBINED_DOMAIN: &str = "fuchsia.driver.lab combined digest v1";
const POLICY_DOMAIN: &str = "fuchsia.driver.lab policy digest v1";

fn put_bytes(hasher: &mut Sha256, tag: &str, bytes: &[u8]) {
    hasher.update((tag.len() as u64).to_le_bytes());
    hasher.update(tag.as_bytes());
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn put_u64(hasher: &mut Sha256, tag: &str, value: u64) {
    put_bytes(hasher, tag, &value.to_le_bytes());
}

fn finish(hasher: Sha256) -> Sha256Digest {
    let output = hasher.finalize();
    Sha256Digest(output.as_slice().try_into().expect("sha256 output is 32 bytes"))
}

/// Computes the canonical digest of one resource description and its
/// applicable ceiling slice.
pub fn resource_digest(inputs: &DigestInputs<'_>) -> Sha256Digest {
    let mut hasher = Sha256::new();
    put_bytes(&mut hasher, "domain", RESOURCE_DOMAIN.as_bytes());
    put_bytes(&mut hasher, "node", inputs.node_identity.as_bytes());
    put_bytes(&mut hasher, "provider", inputs.provider.as_bytes());
    put_u64(&mut hasher, "id", u64::from(inputs.id));
    put_bytes(&mut hasher, "name", inputs.resource.name.as_bytes());
    put_bytes(
        &mut hasher,
        "kind",
        match inputs.resource.kind {
            ResourceKind::Mmio => b"mmio",
            ResourceKind::Gpio => b"gpio",
            ResourceKind::I2c => b"i2c",
            ResourceKind::Spi => b"spi",
            ResourceKind::Interrupt => b"interrupt",
            ResourceKind::Clock => b"clock",
            ResourceKind::Reset => b"reset",
            ResourceKind::Serial => b"serial",
        },
    );
    put_u64(&mut hasher, "logical_size", inputs.resource.logical_size);
    put_u64(&mut hasher, "width", u64::from(WIDTH32));
    match inputs.ceiling {
        None => put_u64(&mut hasher, "ceiling", 0),
        Some(ceiling) => {
            put_u64(&mut hasher, "ceiling", 1);
            put_u64(&mut hasher, "allow_unknown_reads", u64::from(ceiling.allow_unknown_reads));
            put_u64(&mut hasher, "allow_poll", u64::from(ceiling.allow_poll));
            put_u64(&mut hasher, "allow_interrupt", u64::from(ceiling.allow_interrupt));
            let mut denied: Vec<(u64, u64)> =
                ceiling.hard_denied.iter().map(|range| (range.start, range.end)).collect();
            denied.sort_unstable();
            put_u64(&mut hasher, "hard_denied_count", denied.len() as u64);
            for (start, end) in denied {
                put_u64(&mut hasher, "deny_start", start);
                put_u64(&mut hasher, "deny_end", end);
            }
            if let Some(proto) = &ceiling.protocol {
                put_u64(&mut hasher, "protocol_ceiling", 1);
                put_u64(&mut hasher, "max_transfer_size", u64::from(proto.max_transfer_size));
                put_u64(&mut hasher, "allow_mutating", u64::from(proto.allow_mutating));
                for method in &proto.allowed_methods {
                    put_bytes(&mut hasher, "method", method.as_bytes());
                }
            } else {
                put_u64(&mut hasher, "protocol_ceiling", 0);
            }
        }
    }
    finish(hasher)
}

/// Digest over an ordered sequence of per-resource digests, used as the
/// whole-instance resource digest in `Describe`. Callers must supply a
/// deterministic order (for example ascending resource ID).
pub fn combined_digest(digests: impl IntoIterator<Item = Sha256Digest>) -> Sha256Digest {
    let mut hasher = Sha256::new();
    put_bytes(&mut hasher, "domain", COMBINED_DOMAIN.as_bytes());
    for digest in digests {
        put_bytes(&mut hasher, "resource", digest.as_bytes());
    }
    finish(hasher)
}

/// The per-resource digests of one proxy instance, in ascending resource
/// ID order (the order `combined_digest` expects).
pub fn per_resource_digests(
    node_identity: &str,
    provider: &str,
    resources: &BTreeMap<ResourceId, MmioResource>,
    ceiling: &BTreeMap<ResourceId, ResourceCeiling>,
) -> BTreeMap<ResourceId, Sha256Digest> {
    resources
        .iter()
        .map(|(id, resource)| {
            let digest = resource_digest(&DigestInputs {
                node_identity,
                provider,
                id: *id,
                resource,
                ceiling: ceiling.get(id),
            });
            (*id, digest)
        })
        .collect()
}

/// Canonical digest of the immutable target-ceiling content, reported as
/// `policy_digest` in `Describe`. Domain-separated from resource digests
/// so the two never collide, even over identical inputs.
pub fn policy_digest(ceiling: &BTreeMap<ResourceId, ResourceCeiling>) -> Sha256Digest {
    let mut hasher = Sha256::new();
    put_bytes(&mut hasher, "domain", POLICY_DOMAIN.as_bytes());
    put_u64(&mut hasher, "resource_count", ceiling.len() as u64);
    for (id, entry) in ceiling {
        put_u64(&mut hasher, "id", u64::from(*id));
        put_u64(&mut hasher, "allow_unknown_reads", u64::from(entry.allow_unknown_reads));
        put_u64(&mut hasher, "allow_poll", u64::from(entry.allow_poll));
        put_u64(&mut hasher, "allow_interrupt", u64::from(entry.allow_interrupt));
        let mut denied: Vec<(u64, u64)> =
            entry.hard_denied.iter().map(|range| (range.start, range.end)).collect();
        denied.sort_unstable();
        put_u64(&mut hasher, "hard_denied_count", denied.len() as u64);
        for (start, end) in denied {
            put_u64(&mut hasher, "deny_start", start);
            put_u64(&mut hasher, "deny_end", end);
        }
    }
    finish(hasher)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource() -> MmioResource {
        MmioResource {
            name: "ctrl".to_string(),
            kind: ResourceKind::Mmio,
            logical_size: 0x100,
            mapped_size: 0x1000,
        }
    }

    fn ceiling() -> ResourceCeiling {
        ResourceCeiling {
            hard_denied: vec![0x40..0x44, 0x80..0x90],
            allow_unknown_reads: true,
            allow_poll: false,
            writable_registers: vec![],
            protocol: None,
            allow_interrupt: false,
        }
    }

    fn digest_of(resource: &MmioResource, ceiling: Option<&ResourceCeiling>) -> Sha256Digest {
        resource_digest(&DigestInputs {
            node_identity: "dev.sys.platform.example",
            provider: "platform",
            id: 1,
            resource,
            ceiling,
        })
    }

    #[test]
    fn digest_is_deterministic() {
        assert_eq!(
            digest_of(&resource(), Some(&ceiling())),
            digest_of(&resource(), Some(&ceiling()))
        );
    }

    #[test]
    fn display_format() {
        let text = digest_of(&resource(), Some(&ceiling())).to_string();
        assert!(text.starts_with("sha256:"));
        assert_eq!(text.len(), "sha256:".len() + 64);
        assert!(text["sha256:".len()..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn every_stable_field_changes_the_digest() {
        let base = digest_of(&resource(), Some(&ceiling()));

        let mut renamed = resource();
        renamed.name = "ctrl2".to_string();
        assert_ne!(digest_of(&renamed, Some(&ceiling())), base);

        let mut resized = resource();
        resized.logical_size = 0x200;
        assert_ne!(digest_of(&resized, Some(&ceiling())), base);

        let other_node = resource_digest(&DigestInputs {
            node_identity: "dev.sys.platform.other",
            provider: "platform",
            id: 1,
            resource: &resource(),
            ceiling: Some(&ceiling()),
        });
        assert_ne!(other_node, base);

        let other_provider = resource_digest(&DigestInputs {
            node_identity: "dev.sys.platform.example",
            provider: "pci",
            id: 1,
            resource: &resource(),
            ceiling: Some(&ceiling()),
        });
        assert_ne!(other_provider, base);

        let other_id = resource_digest(&DigestInputs {
            node_identity: "dev.sys.platform.example",
            provider: "platform",
            id: 2,
            resource: &resource(),
            ceiling: Some(&ceiling()),
        });
        assert_ne!(other_id, base);
    }

    #[test]
    fn ceiling_changes_the_digest() {
        let base = digest_of(&resource(), Some(&ceiling()));

        let mut no_unknown = ceiling();
        no_unknown.allow_unknown_reads = false;
        assert_ne!(digest_of(&resource(), Some(&no_unknown)), base);

        let mut different_denies = ceiling();
        different_denies.hard_denied = vec![0x40..0x48];
        assert_ne!(digest_of(&resource(), Some(&different_denies)), base);

        assert_ne!(digest_of(&resource(), None), base);
        let empty = ResourceCeiling::default();
        assert_ne!(digest_of(&resource(), Some(&empty)), digest_of(&resource(), None));
    }

    #[test]
    fn hard_denied_order_does_not_matter() {
        let reordered = ResourceCeiling {
            hard_denied: vec![0x80..0x90, 0x40..0x44],
            allow_unknown_reads: true,
            allow_poll: false,
            writable_registers: vec![],
            protocol: None,
            allow_interrupt: false,
        };
        assert_eq!(
            digest_of(&resource(), Some(&reordered)),
            digest_of(&resource(), Some(&ceiling()))
        );
    }

    #[test]
    fn mapped_size_is_excluded() {
        let mut remapped = resource();
        remapped.mapped_size = 0x4000;
        assert_eq!(
            digest_of(&remapped, Some(&ceiling())),
            digest_of(&resource(), Some(&ceiling()))
        );
    }

    #[test]
    fn combined_digest_reflects_members() {
        let a = digest_of(&resource(), Some(&ceiling()));
        let b = digest_of(&resource(), None);
        assert_eq!(combined_digest([a, b]), combined_digest([a, b]));
        assert_ne!(combined_digest([a, b]), combined_digest([a]));
        assert_ne!(combined_digest([a, b]), combined_digest([b, a]));
        assert_eq!(combined_digest([]), combined_digest([]));
    }

    #[test]
    fn per_resource_digests_match_direct_computation() {
        let resources = BTreeMap::from([(1, resource()), (2, resource())]);
        let ceilings = BTreeMap::from([(1, ceiling())]);
        let digests = per_resource_digests("node", "fake", &resources, &ceilings);
        assert_eq!(digests.len(), 2);
        assert_eq!(
            digests[&1],
            resource_digest(&DigestInputs {
                node_identity: "node",
                provider: "fake",
                id: 1,
                resource: &resources[&1],
                ceiling: Some(&ceilings[&1]),
            })
        );
        // Resource 2 has no ceiling entry; its digest reflects that.
        assert_ne!(digests[&1], digests[&2]);
    }

    #[test]
    fn policy_digest_is_domain_separated_and_content_sensitive() {
        let empty = BTreeMap::new();
        // The empty policy digest never equals the empty combined resource
        // digest: a fresh instance reports two distinct identities.
        assert_ne!(policy_digest(&empty).to_string(), combined_digest([]).to_string());

        let one = BTreeMap::from([(1, ceiling())]);
        assert_ne!(policy_digest(&one), policy_digest(&empty));

        let mut narrowed = ceiling();
        narrowed.allow_unknown_reads = false;
        let changed = BTreeMap::from([(1, narrowed)]);
        assert_ne!(policy_digest(&changed), policy_digest(&one));

        let reordered_denies = BTreeMap::from([(
            1,
            ResourceCeiling {
                hard_denied: vec![0x80..0x90, 0x40..0x44],
                allow_unknown_reads: true,
                allow_poll: false,
                writable_registers: vec![],
                protocol: None,
                allow_interrupt: false,
            },
        )]);
        assert_eq!(policy_digest(&reordered_denies), policy_digest(&one));

        let mut poll_allowed = ceiling();
        poll_allowed.allow_poll = true;
        let poll_changed = BTreeMap::from([(1, poll_allowed)]);
        assert_ne!(policy_digest(&poll_changed), policy_digest(&one));

        let mut interrupt_allowed = ceiling();
        interrupt_allowed.allow_interrupt = true;
        let interrupt_changed = BTreeMap::from([(1, interrupt_allowed)]);
        assert_ne!(policy_digest(&interrupt_changed), policy_digest(&one));
    }
}
