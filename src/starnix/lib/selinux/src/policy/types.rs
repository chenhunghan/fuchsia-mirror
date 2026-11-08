// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::num::NonZeroU16;
use std::ops::Index;

use fuchsia_rcu::RcuDroppable;
use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use selinux_policy_derive::{HasPolicyId, Parse, Serialize, Validate};

use super::bitmap::IdSet;
use super::error::{ParseError, SerializeError, ValidateError};
use super::id_type::IdType;
use super::indexed::hash_name;
use super::metadata::PolicyVersion;
use super::parser::{Array, PolicyCursor, PolicyWriter};
use super::traits::{HasName, Parse, PolicyId, Serialize, Validate};
use super::{Policy, U24Index};

/// Tag type for type safety of policy type identifiers.
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq, RcuDroppable)]
pub struct TypeTag;

/// Identifies a type (or type attribute) within a policy.
pub type TypeId = IdType<NonZeroU16, TypeTag>;

/// Set of types that are marked permissive.
pub type PermissiveTypeSet = IdSet<TypeId, true>;

/// Set of types for which permission checks are never audit-logged (for policy versions >=
/// [`PolicyVersion::MIN_NEVERAUDIT`]).
#[derive(Clone, Debug, PartialEq, Eq, Validate)]
pub struct NeverAuditTypeSet(IdSet<TypeId, true>);

impl NeverAuditTypeSet {
    /// Constructs a [`NeverAuditTypeSet`] from an iterator of [`TypeId`]s.
    #[cfg(test)]
    pub fn from_ids<I: IntoIterator<Item = TypeId>>(ids: I) -> Self {
        Self(IdSet::from_ids(ids))
    }

    /// Returns `true` if `id` is in the set.
    #[inline]
    pub fn contains(&self, id: TypeId) -> bool {
        self.0.contains(id)
    }
}

impl Parse for NeverAuditTypeSet {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        if cursor.policy_version() >= PolicyVersion::MIN_NEVERAUDIT {
            Ok(Self(IdSet::parse(cursor)?))
        } else {
            Ok(Self(IdSet::empty()))
        }
    }
}

impl Serialize for NeverAuditTypeSet {
    fn serialize(&self, writer: &mut PolicyWriter<'_>) -> Result<(), SerializeError> {
        if writer.version() >= PolicyVersion::MIN_NEVERAUDIT {
            self.0.serialize(writer)?;
        }
        Ok(())
    }
}

/// Set of [`TypeId`]s.
pub type TypeSet = IdSet<TypeId>;

impl Validate for TypeId {
    fn validate(&self, policy: &Policy) -> Result<(), ValidateError> {
        policy
            .types
            .get_by_id(*self)
            .map(|_| ())
            .ok_or_else(|| ValidateError::UnknownId { kind: "type", id: self.as_u32() })
    }
}

/// Classification of a type symbol (alias, primary type, or attribute).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Parse, Serialize, Validate)]
#[policy(wire_type = u32)]
pub(super) enum TypeKind {
    Alias = 0,
    Type = 1,
    Attribute = 3,
}

/// Parsed SELinux type, containing an ID, a name, properties, and optional bounds.
#[derive(Clone, Debug, Validate, HasPolicyId)]
pub struct Type {
    id: TypeId,
    name: Box<[u8]>,
    properties: TypeKind,
    bounds: Option<TypeId>,
}

impl HasName for Type {
    fn name(&self) -> &[u8] {
        &self.name
    }
}

impl Type {
    #[cfg(test)]
    pub(super) fn new_for_test(
        id: TypeId,
        name: &[u8],
        properties: TypeKind,
        bounds: Option<TypeId>,
    ) -> Self {
        Self { id, name: Box::from(name), properties, bounds }
    }

    pub(super) fn bounded_by(&self) -> Option<TypeId> {
        self.bounds
    }
}

#[derive(Parse, Serialize)]
struct BinaryTypeMetadata {
    length: u32,
    id: TypeId,
    properties: TypeKind,
    bounds: Option<TypeId>,
}

impl Parse for Type {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let metadata = BinaryTypeMetadata::parse(cursor)?;
        let name = Box::from(cursor.read_bytes(metadata.length as usize)?);

        Ok(Self { id: metadata.id, name, properties: metadata.properties, bounds: metadata.bounds })
    }
}

impl Serialize for Type {
    fn serialize(&self, writer: &mut PolicyWriter<'_>) -> Result<(), SerializeError> {
        let metadata = BinaryTypeMetadata {
            length: self.name.len() as u32,
            id: self.id,
            properties: self.properties,
            bounds: self.bounds,
        };
        metadata.serialize(writer)?;
        writer.write_bytes(&self.name);
        Ok(())
    }
}

/// Container for all types in the policy, providing indices for fast lookup by ID and Name.
#[derive(Clone, Debug)]
pub struct Types {
    primary_names_count: u32,
    /// In-order list of all types, attributes, and aliases.
    ordered: Array<Type>,

    /// Maps TypeId -> index in `ordered`. Only contains Types and Attributes.
    /// Index is `TypeId - 1`.
    by_id: Box<[Option<U24Index>]>,

    /// Maps name -> index in `ordered`. Indexes all entries (`Type`, `Alias`, and `Attribute`) to
    /// enforce name uniqueness, while `get_by_name` filters out `Attribute`s.
    by_name: HashTable<U24Index>,
    hasher: rapidhash::RapidBuildHasher,
}

impl Parse for Types {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let primary_names_count = u32::parse(cursor)?;
        let ordered = Array::<Type>::parse(cursor)?;
        if primary_names_count > ordered.len() as u32 {
            return Err(ParseError::InvalidPrimaryNamesCount {
                expected_at_most: ordered.len() as u32,
                found: primary_names_count,
            });
        }
        cursor.set_types_count(primary_names_count);

        // Build indices
        let mut by_id = vec![None; primary_names_count as usize];
        let mut primary_count = 0u32;
        let hasher = rapidhash::RapidBuildHasher::default();
        let mut by_name = HashTable::with_capacity(ordered.len());

        for (index, item) in ordered.iter().enumerate() {
            if item.id.as_u32() > primary_names_count {
                return Err(ParseError::InvalidId { value: item.id.as_u32() });
            }
            let u24_idx: U24Index = index.try_into()?;
            if item.properties == TypeKind::Type || item.properties == TypeKind::Attribute {
                let id = item.id.as_u32() as usize;
                if by_id[id - 1].is_some() {
                    return Err(ParseError::DuplicateId { id: item.id.as_u32() });
                }
                by_id[id - 1] = Some(u24_idx);
                primary_count += 1;
            }
            let name = item.name.as_ref();
            let hash = hash_name(&hasher, name);
            let Entry::Vacant(entry) = by_name.entry(
                hash,
                |&idx| ordered[usize::from(idx)].name.as_ref() == name,
                |&idx| hash_name(&hasher, ordered[usize::from(idx)].name.as_ref()),
            ) else {
                return Err(ParseError::DuplicateName { name: name.into() });
            };
            entry.insert(u24_idx);
        }
        if primary_count != primary_names_count {
            return Err(ParseError::InvalidPrimaryNamesCount {
                expected_at_most: primary_count,
                found: primary_names_count,
            });
        }

        by_id.shrink_to_fit();
        by_name.shrink_to_fit(|&idx| hash_name(&hasher, ordered[usize::from(idx)].name.as_ref()));

        Ok(Self { primary_names_count, ordered, by_id: by_id.into_boxed_slice(), by_name, hasher })
    }
}

impl Serialize for Types {
    fn serialize(&self, writer: &mut PolicyWriter<'_>) -> Result<(), SerializeError> {
        self.primary_names_count.serialize(writer)?;
        self.ordered.serialize(writer)
    }
}

impl Validate for Types {
    fn validate(&self, policy: &Policy) -> Result<(), ValidateError> {
        for item in self.ordered.iter() {
            match item.properties {
                TypeKind::Alias => {
                    let Some(target) = self.get_by_id(item.id) else {
                        return Err(ValidateError::InvalidTypeAlias { id: item.id.as_u32() });
                    };
                    if target.properties != TypeKind::Type || item.bounds.is_some() {
                        return Err(ValidateError::InvalidTypeAlias { id: item.id.as_u32() });
                    }
                }
                TypeKind::Attribute => {
                    if item.bounds.is_some() {
                        return Err(ValidateError::InvalidTypeBounds { id: item.id.as_u32() });
                    }
                }
                TypeKind::Type => {
                    self.validate_bounds(item.id, item.bounds, 1)?;
                }
            }
        }
        self.ordered.validate(policy)
    }
}

impl Types {
    /// Maximum number of types permitted in a `typebounds` hierarchy (matching SELinux's
    /// `POLICYDB_BOUNDS_MAXDEPTH = 4`, i.e. up to 3 `typebounds` links).
    const MAX_TYPEBOUNDS_DEPTH: usize = 4;

    /// Recursively validates the `typebounds` chain starting at `initial_id`.
    fn validate_bounds(
        &self,
        initial_id: TypeId,
        bound: Option<TypeId>,
        depth: usize,
    ) -> Result<(), ValidateError> {
        let Some(bound_id) = bound else {
            return Ok(());
        };
        if bound_id == initial_id || depth >= Self::MAX_TYPEBOUNDS_DEPTH {
            return Err(ValidateError::InvalidTypeBounds { id: initial_id.as_u32() });
        }
        let Some(parent) = self.get_by_id(bound_id) else {
            return Err(ValidateError::InvalidTypeBounds { id: initial_id.as_u32() });
        };
        if parent.properties != TypeKind::Type {
            return Err(ValidateError::InvalidTypeBounds { id: initial_id.as_u32() });
        }
        self.validate_bounds(initial_id, parent.bounded_by(), depth + 1)
    }

    /// Returns the number of primary type and attribute names (excluding aliases).
    pub fn primary_names_count(&self) -> usize {
        self.primary_names_count as usize
    }

    pub(super) fn get_by_id(&self, id: TypeId) -> Option<&Type> {
        let index = self.by_id.get((id.as_u32() - 1) as usize)?.as_ref()?;
        Some(&self.ordered[*index])
    }

    pub fn get_by_name(&self, name: &[u8]) -> Option<&Type> {
        let hash = hash_name(&self.hasher, name);
        let idx = self.by_name.find(hash, |&idx| self.ordered[idx].name.as_ref() == name)?;
        let item = &self.ordered[*idx];
        (item.properties != TypeKind::Attribute).then_some(item)
    }
}

/// Type-to-attribute mappings for each primary type in the policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Validate)]
pub struct TypeAttributeMaps {
    maps: Box<[TypeSet]>,
}

impl Index<TypeId> for TypeAttributeMaps {
    type Output = TypeSet;

    fn index(&self, id: TypeId) -> &Self::Output {
        &self.maps[(id.as_u32() - 1) as usize]
    }
}

impl Parse for TypeAttributeMaps {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let count = cursor.types_count() as usize;
        let maps = cursor.parse_elements(count)?;
        Ok(Self { maps })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::metadata::PolicyVersion;

    #[test]
    fn test_types_lookup() {
        let mut bytes = Vec::new();
        let mut policy_writer = PolicyWriter::new(PolicyVersion::V33, &mut bytes);
        2u32.serialize(&mut policy_writer).unwrap(); // primary_names_count
        2u32.serialize(&mut policy_writer).unwrap(); // ordered count = 2

        let t1 = Type {
            id: TypeId::new(NonZeroU16::new(1).unwrap()),
            name: Box::from(b"foo".as_slice()),
            properties: TypeKind::Type,
            bounds: None,
        };
        t1.serialize(&mut policy_writer).unwrap();

        let t2 = Type {
            id: TypeId::new(NonZeroU16::new(2).unwrap()),
            name: Box::from(b"bar".as_slice()),
            properties: TypeKind::Type,
            bounds: None,
        };
        t2.serialize(&mut policy_writer).unwrap();

        let mut cursor = PolicyCursor::new(&bytes);
        let types = Types::parse(&mut cursor).expect("parse types");

        assert_eq!(
            types.get_by_id(TypeId::new(NonZeroU16::new(1).unwrap())).map(|t| t.name()),
            Some(b"foo".as_slice())
        );
        assert_eq!(
            types.get_by_id(TypeId::new(NonZeroU16::new(2).unwrap())).map(|t| t.name()),
            Some(b"bar".as_slice())
        );
        assert!(types.get_by_id(TypeId::new(NonZeroU16::new(3).unwrap())).is_none());

        assert_eq!(
            types.get_by_name(b"foo").map(|t| t.id),
            Some(TypeId::new(NonZeroU16::new(1).unwrap()))
        );
        assert_eq!(
            types.get_by_name(b"bar").map(|t| t.id),
            Some(TypeId::new(NonZeroU16::new(2).unwrap()))
        );
        assert!(types.get_by_name(b"baz").is_none());
    }

    #[test]
    fn test_types_duplicate_id() {
        let mut bytes = Vec::new();
        let mut policy_writer = PolicyWriter::new(PolicyVersion::V33, &mut bytes);
        2u32.serialize(&mut policy_writer).unwrap(); // primary_names_count
        2u32.serialize(&mut policy_writer).unwrap(); // ordered count = 2

        let t1 = Type {
            id: TypeId::new(NonZeroU16::new(1).unwrap()),
            name: Box::from(b"foo".as_slice()),
            properties: TypeKind::Type,
            bounds: None,
        };
        t1.serialize(&mut policy_writer).unwrap();

        let t2 = Type {
            id: TypeId::new(NonZeroU16::new(1).unwrap()),
            name: Box::from(b"bar".as_slice()),
            properties: TypeKind::Type,
            bounds: None,
        };
        t2.serialize(&mut policy_writer).unwrap();

        let mut cursor = PolicyCursor::new(&bytes);
        let result = Types::parse(&mut cursor);
        assert!(matches!(result, Err(ParseError::DuplicateId { id: 1 })));
    }

    #[test]
    fn test_types_duplicate_name() {
        let mut bytes = Vec::new();
        let mut policy_writer = PolicyWriter::new(PolicyVersion::V33, &mut bytes);
        2u32.serialize(&mut policy_writer).unwrap(); // primary_names_count
        2u32.serialize(&mut policy_writer).unwrap(); // ordered count = 2

        let t1 = Type {
            id: TypeId::new(NonZeroU16::new(1).unwrap()),
            name: Box::from(b"foo".as_slice()),
            properties: TypeKind::Type,
            bounds: None,
        };
        t1.serialize(&mut policy_writer).unwrap();

        let t2 = Type {
            id: TypeId::new(NonZeroU16::new(2).unwrap()),
            name: Box::from(b"foo".as_slice()),
            properties: TypeKind::Type,
            bounds: None,
        };
        t2.serialize(&mut policy_writer).unwrap();

        let mut cursor = PolicyCursor::new(&bytes);
        let result = Types::parse(&mut cursor);
        assert!(matches!(result, Err(ParseError::DuplicateName { name }) if name == b"foo"));
    }

    #[test]
    fn test_types_duplicate_alias_name() {
        let mut bytes = Vec::new();
        let mut policy_writer = PolicyWriter::new(PolicyVersion::V33, &mut bytes);
        1u32.serialize(&mut policy_writer).unwrap(); // primary_names_count
        2u32.serialize(&mut policy_writer).unwrap(); // ordered count = 2

        let t1 = Type {
            id: TypeId::new(NonZeroU16::new(1).unwrap()),
            name: Box::from(b"foo".as_slice()),
            properties: TypeKind::Type,
            bounds: None,
        };
        t1.serialize(&mut policy_writer).unwrap();

        let t2 = Type {
            id: TypeId::new(NonZeroU16::new(1).unwrap()),
            name: Box::from(b"foo".as_slice()),
            properties: TypeKind::Alias,
            bounds: None,
        };
        t2.serialize(&mut policy_writer).unwrap();

        let mut cursor = PolicyCursor::new(&bytes);
        let result = Types::parse(&mut cursor);
        assert!(matches!(result, Err(ParseError::DuplicateName { name }) if name == b"foo"));
    }

    #[test]
    fn test_types_rejects_id_exceeding_primary_names_count() {
        let mut bytes = Vec::new();
        let mut policy_writer = PolicyWriter::new(PolicyVersion::V33, &mut bytes);
        1u32.serialize(&mut policy_writer).unwrap(); // primary_names_count = 1
        1u32.serialize(&mut policy_writer).unwrap(); // ordered count = 1

        let t1 = Type {
            id: TypeId::new(NonZeroU16::new(2).unwrap()),
            name: Box::from(b"oob_type".as_slice()),
            properties: TypeKind::Type,
            bounds: None,
        };
        t1.serialize(&mut policy_writer).unwrap();

        let mut cursor = PolicyCursor::new(&bytes);
        let result = Types::parse(&mut cursor);
        assert!(matches!(result, Err(ParseError::InvalidId { value: 2 })));
    }

    #[test]
    fn test_types_cyclic_typebounds_parsed_and_bounded() {
        // Construct two primary types with a mutual 2-cycle: t1 -> t2 -> t1.
        let mut bytes = Vec::new();
        let mut policy_writer = PolicyWriter::new(PolicyVersion::V33, &mut bytes);
        2u32.serialize(&mut policy_writer).unwrap();
        2u32.serialize(&mut policy_writer).unwrap();

        let t1 = Type {
            id: TypeId::new(NonZeroU16::new(1).unwrap()),
            name: Box::from(b"t1".as_slice()),
            properties: TypeKind::Type,
            bounds: Some(TypeId::new(NonZeroU16::new(2).unwrap())),
        };
        t1.serialize(&mut policy_writer).unwrap();

        let t2 = Type {
            id: TypeId::new(NonZeroU16::new(2).unwrap()),
            name: Box::from(b"t2".as_slice()),
            properties: TypeKind::Type,
            bounds: Some(TypeId::new(NonZeroU16::new(1).unwrap())),
        };
        t2.serialize(&mut policy_writer).unwrap();

        let mut cursor = PolicyCursor::new(&bytes);
        let parsed = Types::parse(&mut cursor).unwrap();
        let policy = Policy::parse(include_bytes!(
            "../../testdata/composite_policies/compiled/minimal_policy"
        ))
        .unwrap();
        // Verify get_by_id works and cyclic bounds are rejected during validation.
        let first = parsed.get_by_id(TypeId::new(NonZeroU16::new(1).unwrap())).unwrap();
        let second = parsed.get_by_id(first.bounded_by().unwrap()).unwrap();
        assert_eq!(second.bounded_by(), Some(first.id));
        assert_eq!(parsed.validate(&policy), Err(ValidateError::InvalidTypeBounds { id: 1 }));

        // Verify an indirect cycle (t1 -> t2 -> t3 -> t2) is also rejected by `Types::validate`.
        let mut indirect_bytes = Vec::new();
        let mut indirect_writer = PolicyWriter::new(PolicyVersion::V33, &mut indirect_bytes);
        3u32.serialize(&mut indirect_writer).unwrap();
        3u32.serialize(&mut indirect_writer).unwrap();
        for (id, name, bound) in [(1, b"t1", 2), (2, b"t2", 3), (3, b"t3", 2)] {
            let t = Type {
                id: TypeId::new(NonZeroU16::new(id).unwrap()),
                name: Box::from(name.as_slice()),
                properties: TypeKind::Type,
                bounds: Some(TypeId::new(NonZeroU16::new(bound).unwrap())),
            };
            t.serialize(&mut indirect_writer).unwrap();
        }
        let mut indirect_cursor = PolicyCursor::new(&indirect_bytes);
        let indirect_parsed = Types::parse(&mut indirect_cursor).unwrap();
        assert_eq!(
            indirect_parsed.validate(&policy),
            Err(ValidateError::InvalidTypeBounds { id: 1 })
        );

        // Verify a chain of `MAX_TYPEBOUNDS_DEPTH` types (3 links) succeeds via `Types::validate`,
        // while a chain of `MAX_TYPEBOUNDS_DEPTH + 1` types (4 links) is rejected.
        let parse_chain = |chain_len: u16| {
            let mut chain_bytes = Vec::new();
            let mut chain_writer = PolicyWriter::new(PolicyVersion::V33, &mut chain_bytes);
            (chain_len as u32).serialize(&mut chain_writer).unwrap();
            (chain_len as u32).serialize(&mut chain_writer).unwrap();
            for i in 1..=chain_len {
                let t = Type {
                    id: TypeId::new(NonZeroU16::new(i).unwrap()),
                    name: format!("deep_{i}").into_bytes().into_boxed_slice(),
                    properties: TypeKind::Type,
                    bounds: (i < chain_len).then(|| TypeId::new(NonZeroU16::new(i + 1).unwrap())),
                };
                t.serialize(&mut chain_writer).unwrap();
            }
            let mut chain_cursor = PolicyCursor::new(&chain_bytes);
            Types::parse(&mut chain_cursor).unwrap()
        };
        let valid_chain = parse_chain(Types::MAX_TYPEBOUNDS_DEPTH as u16);
        assert_eq!(valid_chain.validate(&policy), Ok(()));

        let too_deep_chain = parse_chain((Types::MAX_TYPEBOUNDS_DEPTH + 1) as u16);
        assert_eq!(
            too_deep_chain.validate(&policy),
            Err(ValidateError::InvalidTypeBounds { id: 1 })
        );
    }

    #[test]
    fn test_types_validate_rejects_invalid_aliases_and_attribute_bounds() {
        let policy = Policy::parse(include_bytes!(
            "../../testdata/composite_policies/compiled/minimal_policy"
        ))
        .unwrap();
        let parse_types = |primary_names_count: u32, items: &[Type]| {
            let mut bytes = Vec::new();
            let mut writer = PolicyWriter::new(PolicyVersion::V33, &mut bytes);
            primary_names_count.serialize(&mut writer).unwrap();
            (items.len() as u32).serialize(&mut writer).unwrap();
            for item in items {
                item.serialize(&mut writer).unwrap();
            }
            let mut cursor = PolicyCursor::new(&bytes);
            Types::parse(&mut cursor).unwrap()
        };
        let id1 = TypeId::new(NonZeroU16::new(1).unwrap());
        let id2 = TypeId::new(NonZeroU16::new(2).unwrap());

        // Alias targeting an Attribute is rejected with InvalidTypeAlias.
        let alias_to_attr = parse_types(
            1,
            &[
                Type {
                    id: id1,
                    name: Box::from(b"attr1".as_slice()),
                    properties: TypeKind::Attribute,
                    bounds: None,
                },
                Type {
                    id: id1,
                    name: Box::from(b"alias1".as_slice()),
                    properties: TypeKind::Alias,
                    bounds: None,
                },
            ],
        );
        assert_eq!(alias_to_attr.validate(&policy), Err(ValidateError::InvalidTypeAlias { id: 1 }));

        // Alias with bounds is rejected with InvalidTypeAlias.
        let alias_with_bounds = parse_types(
            2,
            &[
                Type {
                    id: id1,
                    name: Box::from(b"type1".as_slice()),
                    properties: TypeKind::Type,
                    bounds: None,
                },
                Type {
                    id: id2,
                    name: Box::from(b"type2".as_slice()),
                    properties: TypeKind::Type,
                    bounds: None,
                },
                Type {
                    id: id1,
                    name: Box::from(b"alias1".as_slice()),
                    properties: TypeKind::Alias,
                    bounds: Some(id2),
                },
            ],
        );
        assert_eq!(
            alias_with_bounds.validate(&policy),
            Err(ValidateError::InvalidTypeAlias { id: 1 })
        );

        // Attribute with bounds is rejected with InvalidTypeBounds.
        let attr_with_bounds = parse_types(
            2,
            &[
                Type {
                    id: id1,
                    name: Box::from(b"attr1".as_slice()),
                    properties: TypeKind::Attribute,
                    bounds: Some(id2),
                },
                Type {
                    id: id2,
                    name: Box::from(b"type2".as_slice()),
                    properties: TypeKind::Type,
                    bounds: None,
                },
            ],
        );
        assert_eq!(
            attr_with_bounds.validate(&policy),
            Err(ValidateError::InvalidTypeBounds { id: 1 })
        );

        // Type bounded by an Attribute is rejected with InvalidTypeBounds.
        let type_bounded_by_attr = parse_types(
            2,
            &[
                Type {
                    id: id1,
                    name: Box::from(b"attr1".as_slice()),
                    properties: TypeKind::Attribute,
                    bounds: None,
                },
                Type {
                    id: id2,
                    name: Box::from(b"type2".as_slice()),
                    properties: TypeKind::Type,
                    bounds: Some(id1),
                },
            ],
        );
        assert_eq!(
            type_bounded_by_attr.validate(&policy),
            Err(ValidateError::InvalidTypeBounds { id: 2 })
        );
    }
}
