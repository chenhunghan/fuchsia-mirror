// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::object_record::{
    AttributeId, AttributeKey, ObjectKey, ObjectKeyData, ObjectValue, ProjectProperty,
};
use super::{Extent, ExtentValue};
use crate::log::*;
use crate::lsm_tree::merge::ItemOp::{Discard, Keep, Replace};
use crate::lsm_tree::merge::{MergeLayerIterator, MergeResult};
use crate::lsm_tree::types::Item;

fn merge_extents(
    object_id: u64,
    attribute_id: AttributeId,
    left: &MergeLayerIterator<'_, ObjectKey, ObjectValue>,
    right: &MergeLayerIterator<'_, ObjectKey, ObjectValue>,
    left_key: &Extent,
    right_key: &Extent,
    left_value: &ExtentValue,
    right_value: &ExtentValue,
) -> MergeResult<ObjectKey, ObjectValue> {
    debug_assert!(right.layer_index != left.layer_index);

    if left_key.end <= right_key.start {
        // Extents don't overlap.
        return MergeResult::EmitLeft;
    }

    // The start of the left extent is <= the start of the right extent, due to merge key ordering.
    //
    // One of the extents has to win. The way we break this tie is by picking the extent from the
    // newest layer (i.e. the layer with the lowest index).
    //
    // Generally, we'll be doing the following:
    //
    //  Old  |----------|
    //  New          |----------|
    //
    // Turns into
    //
    //  Emit  |------|
    //  Old          |--|
    //  New          |----------|

    if right.layer_index < left.layer_index {
        // Right layer is newer.
        debug_assert!(left_key.start < right_key.start);
        return MergeResult::Other {
            emit: Some(
                Item::new(
                    ObjectKey::extent(object_id, attribute_id, left_key.start..right_key.start),
                    ObjectValue::Extent(
                        left_value.shrunk(
                            left_key.end - left_key.start,
                            right_key.start - left_key.start,
                        ),
                    ),
                )
                .boxed(),
            ),
            left: Replace(
                Item::new(
                    ObjectKey::extent(object_id, attribute_id, right_key.start..left_key.end),
                    ObjectValue::Extent(left_value.offset_by(
                        right_key.start - left_key.start,
                        left_key.end - left_key.start,
                    )),
                )
                .boxed(),
            ),
            right: Keep,
        };
    }
    // Left layer is newer.
    if left_key.end >= right_key.end {
        // The left key entirely contains the right key.
        return MergeResult::Other { emit: None, left: Keep, right: Discard };
    }
    MergeResult::Other {
        emit: None,
        left: Keep,
        right: Replace(
            Item::new(
                ObjectKey::extent(object_id, attribute_id, left_key.end..right_key.end),
                ObjectValue::Extent(
                    right_value
                        .offset_by(left_key.end - right_key.start, right_key.end - right_key.start),
                ),
            )
            .boxed(),
        ),
    }
}

/// Merge function for items in the object store.
///
/// The most interesting behaviour in this merge function is how extents are handled. Since extents
/// can overlap and replace one another, the merge function generally builds up the most
/// recent view of the extents in the tree, so that the output of a full merge contains no
/// overlapping extents. You can imagine looking down at the extents from the top-most layer.
///
/// A brief example:
///
/// Layer 0   |a-a-a-a|     |b-b-b|
/// Layer 1   |c-c-c-c-c|
/// Layer 2                     |d-d-d-d|
///
/// Merged    |a-a-a-a|c|   |b-b-b|d-d-d|
///
/// `Merger` emits non-overlapping segments without coalescing across layers; single-layer
/// `merge_into` insertions and compaction streams coalesce adjacent compatible records via
/// `Coalesce` / `CoalescingIterator`.
pub fn merge(
    left: &MergeLayerIterator<'_, ObjectKey, ObjectValue>,
    right: &MergeLayerIterator<'_, ObjectKey, ObjectValue>,
) -> MergeResult<ObjectKey, ObjectValue> {
    if left.key().object_id != right.key().object_id {
        return MergeResult::EmitLeft;
    }
    match (left.key(), right.key(), left.value(), right.value()) {
        (
            ObjectKey {
                object_id,
                data: ObjectKeyData::Attribute(left_attr_id, AttributeKey::Extent(left_extent_key)),
            },
            ObjectKey {
                object_id: _,
                data:
                    ObjectKeyData::Attribute(right_attr_id, AttributeKey::Extent(right_extent_key)),
            },
            ObjectValue::Extent(left_extent),
            ObjectValue::Extent(right_extent),
        ) if left_attr_id == right_attr_id => {
            return merge_extents(
                *object_id,
                *left_attr_id,
                left,
                right,
                left_extent_key,
                right_extent_key,
                left_extent,
                right_extent,
            );
        }
        (
            left_key,
            right_key,
            ObjectValue::BytesAndNodes(left_usage),
            ObjectValue::BytesAndNodes(right_usage),
        ) if matches!(
            left_key.data,
            ObjectKeyData::Project { property: ProjectProperty::Usage, .. }
                | ObjectKeyData::PosixOwner { .. }
        ) && left_key == right_key =>
        {
            let result = *left_usage + *right_usage;
            // Tombstone the tracking when it goes to zero.
            if result.is_zero() {
                MergeResult::Other { emit: None, left: Discard, right: Discard }
            } else {
                MergeResult::Other {
                    emit: None,
                    left: Discard,
                    right: Replace(
                        Item::new(right.key().clone(), ObjectValue::BytesAndNodes(result)).boxed(),
                    ),
                }
            }
        }
        // Tombstones (ObjectKeyData::Object) compare before others, so always appear on left.
        (ObjectKey { data: ObjectKeyData::Object, .. }, _, ObjectValue::None, _) => {
            if left.layer_index > right.layer_index {
                warn!("Detected inconsistency: record has been inserted after tombstone");
                MergeResult::EmitLeft
            } else {
                MergeResult::Other { emit: None, left: Keep, right: Discard }
            }
        }
        // Note that identical keys are sorted by layer_index, so left is always newer.
        (left_key, right_key, _, _) if left_key == right_key => {
            debug_assert!(left.layer_index < right.layer_index);
            MergeResult::Other { emit: None, left: Keep, right: Discard }
        }
        _ => MergeResult::EmitLeft,
    }
}

#[cfg(test)]
mod tests {
    use super::merge;
    use crate::checksum::Checksums;
    use crate::lsm_tree::persistent_layer::{
        BloomFilterOptions, PersistentLayer, PersistentLayerWriter,
    };
    use crate::lsm_tree::types::{
        Item, LayerIterator, LayerWriter, MaybeContainsKey, MergeableKey, Value,
    };
    use crate::lsm_tree::{LSMTree, LockedLayer, Query};
    use crate::object_store::extent::MIN_BLOCK_SIZE;
    use crate::object_store::extent_record::ExtentValue;
    use crate::object_store::object_record::{
        AttributeKey, BytesAndNodes, ObjectKey, ObjectValue, PosixOwner, Timestamp,
    };
    use crate::object_store::tree::MajorCompactable;
    use crate::object_store::{AttributeId, ProjectId, VOLUME_DATA_KEY_ID};
    use crate::testing::fake_object::{FakeObject, FakeObjectHandle};
    use crate::testing::writer::Writer;
    use anyhow::Error;
    use std::sync::Arc;
    use storage_units::BlockSize;

    async fn test_merge<K: MergeableKey, V: Value + PartialEq>(
        tree: &LSMTree<K, V>,
        layer0: &[Item<K, V>],
        layer1: &[Item<K, V>],
        expected: &[Item<K, V>],
    ) {
        for item in layer1 {
            tree.insert(item.clone()).expect("insert error");
        }
        tree.seal();
        for item in layer0 {
            tree.insert(item.clone()).expect("insert error");
        }
        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await.expect("seek failed");
        for e in expected {
            assert_eq!(iter.get().expect("get failed"), e.as_item_ref());
            iter.advance().await.expect("advance failed");
        }
        assert!(iter.get().is_none());
    }

    #[fuchsia::test]
    async fn test_merge_extents_non_overlapping() -> Result<(), Error> {
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..512),
            ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID)),
        ))
        .expect("insert error");
        tree.seal();

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 512..1024),
            ObjectValue::Extent(ExtentValue::new_raw(16384, VOLUME_DATA_KEY_ID)),
        ))
        .expect("insert error");

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..512));
        iter.advance().await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 512..1024));
        iter.advance().await?;
        assert!(iter.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_extents_rewrite_right() -> Result<(), Error> {
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..1024),
            ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID)),
        ))
        .expect("insert error");
        tree.seal();

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 512..1024),
            ObjectValue::Extent(ExtentValue::new_raw(16384, VOLUME_DATA_KEY_ID)),
        ))
        .expect("insert error");

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..512));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID))
        );
        iter.advance().await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 512..1024));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::new_raw(16384, VOLUME_DATA_KEY_ID))
        );
        iter.advance().await?;
        assert!(iter.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_extents_rewrite_left() -> Result<(), Error> {
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..1024),
            ObjectValue::Extent(ExtentValue::with_checksum(
                0,
                Checksums::fletcher(vec![1, 2]),
                VOLUME_DATA_KEY_ID,
            )),
        ))
        .expect("insert error");
        tree.seal();

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..512),
            ObjectValue::Extent(ExtentValue::with_checksum(
                16384,
                Checksums::fletcher(vec![3]),
                VOLUME_DATA_KEY_ID,
            )),
        ))
        .expect("insert error");

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..512));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::with_checksum(
                16384,
                Checksums::fletcher(vec![3]),
                VOLUME_DATA_KEY_ID
            ))
        );
        iter.advance().await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 512..1024));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::with_checksum(
                512,
                Checksums::fletcher(vec![2]),
                VOLUME_DATA_KEY_ID
            ))
        );
        iter.advance().await?;
        assert!(iter.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_extents_rewrite_middle() -> Result<(), Error> {
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..2048),
            ObjectValue::Extent(ExtentValue::with_checksum(
                0,
                Checksums::fletcher(vec![1, 2, 3, 4]),
                VOLUME_DATA_KEY_ID,
            )),
        ))
        .expect("insert error");
        tree.seal();

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 1024..1536),
            ObjectValue::Extent(ExtentValue::with_checksum(
                16384,
                Checksums::fletcher(vec![5]),
                VOLUME_DATA_KEY_ID,
            )),
        ))
        .expect("insert error");

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..1024));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::with_checksum(
                0,
                Checksums::fletcher(vec![1, 2]),
                VOLUME_DATA_KEY_ID
            ))
        );
        iter.advance().await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 1024..1536));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::with_checksum(
                16384,
                Checksums::fletcher(vec![5]),
                VOLUME_DATA_KEY_ID
            ))
        );
        iter.advance().await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 1536..2048));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::with_checksum(
                1536,
                Checksums::fletcher(vec![4]),
                VOLUME_DATA_KEY_ID
            ))
        );
        iter.advance().await?;
        assert!(iter.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_extents_rewrite_eclipses() -> Result<(), Error> {
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 1024..1536),
            ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID)),
        ))
        .expect("insert error");
        tree.seal();

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..2048),
            ObjectValue::Extent(ExtentValue::new_raw(16384, VOLUME_DATA_KEY_ID)),
        ))
        .expect("insert error");

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..2048));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::new_raw(16384, VOLUME_DATA_KEY_ID))
        );
        iter.advance().await?;
        assert!(iter.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_extents_delete_left() -> Result<(), Error> {
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..1024),
            ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID)),
        ))
        .expect("insert error");
        tree.seal();

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..512),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..512));
        assert_eq!(iter.get().unwrap().value, &ObjectValue::deleted_extent());
        iter.advance().await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 512..1024));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::new_raw(512, VOLUME_DATA_KEY_ID))
        );
        iter.advance().await?;
        assert!(iter.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_extents_delete_right() -> Result<(), Error> {
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..1024),
            ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID)),
        ))
        .expect("insert error");
        tree.seal();

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 512..1024),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..512));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID))
        );
        iter.advance().await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 512..1024));
        assert_eq!(iter.get().unwrap().value, &ObjectValue::deleted_extent());
        iter.advance().await?;
        assert!(iter.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_extents_delete_middle() -> Result<(), Error> {
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..2048),
            ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID)),
        ))
        .expect("insert error");
        tree.seal();

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 1024..1536),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..1024));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID))
        );
        iter.advance().await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 1024..1536));
        assert_eq!(iter.get().unwrap().value, &ObjectValue::deleted_extent());
        iter.advance().await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 1536..2048));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::new_raw(1536, VOLUME_DATA_KEY_ID))
        );
        iter.advance().await?;
        assert!(iter.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_extents_delete_eclipses() -> Result<(), Error> {
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 1024..1536),
            ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID)),
        ))
        .expect("insert error");
        tree.seal();

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..2048),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..2048));
        assert_eq!(iter.get().unwrap().value, &ObjectValue::deleted_extent());
        iter.advance().await?;
        assert!(iter.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_deleted_extents_new_layer_joins_two_deletions() -> Result<(), Error> {
        // Old layer:  [----]    [----]
        // New layer:       [----]
        // Merger:     [----][---][---]
        // Coalesced:  [--------------]
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..512),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 1024..1536),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");
        tree.seal();

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 512..1024),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");
        tree.seal();

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        for range in [0..512, 512..1024, 1024..1536] {
            assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, range));
            assert_eq!(iter.get().unwrap().value, &ObjectValue::deleted_extent());
            iter.advance().await?;
        }
        assert!(iter.get().is_none());

        let mut coalesced = merger.query(Query::FullScan).await?.coalesce().await?;
        assert_eq!(coalesced.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..1536));
        assert_eq!(coalesced.get().unwrap().value, &ObjectValue::deleted_extent());
        coalesced.advance().await?;
        assert!(coalesced.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_deleted_extents_new_layer_joined_by_old_deletion() -> Result<(), Error> {
        // Old layer:       [----]
        // New layer:  [----]    [----]
        // Merger:     [----][---][---]
        // Coalesced:  [--------------]
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 512..1024),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");
        tree.seal();

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..512),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 1024..1536),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");
        tree.seal();

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        for range in [0..512, 512..1024, 1024..1536] {
            assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, range));
            assert_eq!(iter.get().unwrap().value, &ObjectValue::deleted_extent());
            iter.advance().await?;
        }
        assert!(iter.get().is_none());

        let mut coalesced = merger.query(Query::FullScan).await?.coalesce().await?;
        assert_eq!(coalesced.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..1536));
        assert_eq!(coalesced.get().unwrap().value, &ObjectValue::deleted_extent());
        coalesced.advance().await?;
        assert!(coalesced.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_deleted_extents_overlapping_newest_on_right() -> Result<(), Error> {
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..1024),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");
        tree.seal();

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 512..1536),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        for range in [0..512, 512..1536] {
            assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, range));
            assert_eq!(iter.get().unwrap().value, &ObjectValue::deleted_extent());
            iter.advance().await?;
        }
        assert!(iter.get().is_none());

        let mut coalesced = merger.query(Query::FullScan).await?.coalesce().await?;
        assert_eq!(coalesced.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..1536));
        assert_eq!(coalesced.get().unwrap().value, &ObjectValue::deleted_extent());
        coalesced.advance().await?;
        assert!(coalesced.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_deleted_extents_overlapping_newest_on_left() -> Result<(), Error> {
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 512..1536),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");
        tree.seal();
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..1024),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        for range in [0..1024, 1024..1536] {
            assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, range));
            assert_eq!(iter.get().unwrap().value, &ObjectValue::deleted_extent());
            iter.advance().await?;
        }
        assert!(iter.get().is_none());

        let mut coalesced = merger.query(Query::FullScan).await?.coalesce().await?;
        assert_eq!(coalesced.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..1536));
        assert_eq!(coalesced.get().unwrap().value, &ObjectValue::deleted_extent());
        coalesced.advance().await?;
        assert!(coalesced.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_deleted_extents_new_layer_contained_in_old() -> Result<(), Error> {
        // Old layer:  [--------------]
        // New layer:       [----]
        // Merger:     [----][---][---]
        // Coalesced:  [--------------]
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..1536),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");
        tree.seal();

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 512..1024),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");
        tree.seal();

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        for range in [0..512, 512..1024, 1024..1536] {
            assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, range));
            assert_eq!(iter.get().unwrap().value, &ObjectValue::deleted_extent());
            iter.advance().await?;
        }
        assert!(iter.get().is_none());

        let mut coalesced = merger.query(Query::FullScan).await?.coalesce().await?;
        assert_eq!(coalesced.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..1536));
        assert_eq!(coalesced.get().unwrap().value, &ObjectValue::deleted_extent());
        coalesced.advance().await?;
        assert!(coalesced.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_deleted_extents_new_layer_eclipses_old() -> Result<(), Error> {
        // Old layer:       [----]
        // New layer:  [--------------]
        // Merged:     [--------------]
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 512..1024),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");
        tree.seal();

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..1536),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");
        tree.seal();

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..1536));
        assert_eq!(iter.get().unwrap().value, &ObjectValue::deleted_extent());
        iter.advance().await?;
        assert!(iter.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_deleted_extents_does_not_coalesce_if_not_adjacent_layers()
    -> Result<(), Error> {
        // Layer 0:  [XXXXX]
        // Layer 1:  [--------------]
        // Layer 2:        [XXXXXXXX]
        //  Merged:  [XXXXX|--------]
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::<ObjectKey, ObjectValue>::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 512..1024),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");
        tree.seal();

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..1024),
            ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID)),
        ))
        .expect("insert error");
        tree.seal();

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..512),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..512));
        assert_eq!(iter.get().unwrap().value, &ObjectValue::deleted_extent());
        iter.advance().await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 512..1024));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::new_raw(512, VOLUME_DATA_KEY_ID))
        );
        iter.advance().await?;
        assert!(iter.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_deleted_extents_does_not_coalesce_if_not_adjacent_deletions()
    -> Result<(), Error> {
        // Layer 0:  [XXXXX|--------]
        // Layer 1:           [XXXXX]
        //  Merged:  [XXXXX|--------]
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::<ObjectKey, ObjectValue>::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 1024..1536),
            ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID)),
        ))
        .expect("insert error");
        tree.seal();

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..512),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 512..1536),
            ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID)),
        ))
        .expect("insert error");

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..512));
        assert_eq!(iter.get().unwrap().value, &ObjectValue::deleted_extent());
        iter.advance().await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 512..1536));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID))
        );
        iter.advance().await?;
        assert!(iter.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_deleted_extent_into_overwrites_extents() -> Result<(), Error> {
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::<ObjectKey, ObjectValue>::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..1024),
            ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID)),
        ))
        .expect("insert error");
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 1024..2048),
            ObjectValue::Extent(ExtentValue::new_raw(16384, VOLUME_DATA_KEY_ID)),
        ))
        .expect("insert error");
        let key = ObjectKey::extent(object_id, attr_id, 512..1536);
        tree.merge_into(
            Item::new(key.clone(), ObjectValue::deleted_extent()),
            &key.key_for_merge_into(),
        );

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..512));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID))
        );
        iter.advance().await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 512..1536));
        assert_eq!(iter.get().unwrap().value, &ObjectValue::deleted_extent());
        iter.advance().await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 1536..2048));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::new_raw(16896, VOLUME_DATA_KEY_ID))
        );
        iter.advance().await?;
        assert!(iter.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_deleted_extent_into_merges_with_other_deletions() -> Result<(), Error> {
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::<ObjectKey, ObjectValue>::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..1024),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 1024..2048),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");

        let key = ObjectKey::extent(object_id, attr_id, 512..1536);
        tree.merge_into(
            Item::new(key.clone(), ObjectValue::deleted_extent()),
            &key.key_for_merge_into(),
        );

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..2048));
        assert_eq!(iter.get().unwrap().value, &ObjectValue::deleted_extent());
        iter.advance().await?;
        assert!(iter.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_contiguous_raw_extent_into_preserves_left_offset() -> Result<(), Error> {
        let object_id = 0;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::<ObjectKey, ObjectValue>::new(merge, None);

        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..1024),
            ObjectValue::Extent(ExtentValue::new_raw(50000, VOLUME_DATA_KEY_ID)),
        ))?;
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 1024..2048),
            ObjectValue::Extent(ExtentValue::new_raw(90000, VOLUME_DATA_KEY_ID)),
        ))?;

        let key = ObjectKey::extent(object_id, attr_id, 1024..1536);
        tree.merge_into(
            Item::new(
                key.clone(),
                ObjectValue::Extent(ExtentValue::new_raw(51024, VOLUME_DATA_KEY_ID)),
            ),
            &key.key_for_merge_into(),
        );
        assert_eq!(tree.mutable_layer().len(), 2);

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..1536));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::new_raw(50000, VOLUME_DATA_KEY_ID))
        );
        iter.advance().await?;
        assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 1536..2048));
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::Extent(ExtentValue::new_raw(90512, VOLUME_DATA_KEY_ID))
        );
        iter.advance().await?;
        assert!(iter.get().is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_size_records() {
        let left = &[Item::new(
            ObjectKey::attribute(1, AttributeId::TEST_ID, AttributeKey::Attribute),
            ObjectValue::attribute(5, false),
        )];
        let right = &[Item::new(
            ObjectKey::attribute(1, AttributeId::TEST_ID, AttributeKey::Attribute),
            ObjectValue::attribute(10, false),
        )];
        let tree = LSMTree::new(merge, None);
        test_merge(&tree, left, right, left).await;
    }

    #[fuchsia::test]
    async fn test_different_attributes_not_merged() {
        let left = Item::new(
            ObjectKey::attribute(1, AttributeId::TEST_ID, AttributeKey::Attribute),
            ObjectValue::attribute(5, false),
        );
        let right = Item::new(
            ObjectKey::attribute(1, AttributeId::TEST_ID.next(), AttributeKey::Attribute),
            ObjectValue::attribute(10, false),
        );
        let tree = LSMTree::new(merge, None);
        test_merge(&tree, &[left.clone()], &[right.clone()], &[left, right]).await;

        let left = Item::new(
            ObjectKey::extent(1, AttributeId::TEST_ID, 0..100),
            ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID)),
        );
        let right = Item::new(
            ObjectKey::extent(1, AttributeId::TEST_ID.next(), 0..100),
            ObjectValue::Extent(ExtentValue::new_raw(1, VOLUME_DATA_KEY_ID)),
        );
        let tree = LSMTree::new(merge, None);
        test_merge(&tree, &[left.clone()], &[right.clone()], &[left, right]).await;
    }

    #[fuchsia::test]
    async fn test_tombstone_discards_all_other_records() {
        let tombstone = Item::new(ObjectKey::object(1), ObjectValue::None);
        let other_object = Item::new(
            ObjectKey::object(2),
            ObjectValue::file(
                1,
                0,
                Timestamp::default(),
                Timestamp::default(),
                Timestamp::default(),
                Timestamp::default(),
                None,
                None,
            ),
        );
        let tree = LSMTree::new(merge, None);
        test_merge(
            &tree,
            &[tombstone.clone()],
            &[
                Item::new(
                    ObjectKey::object(1),
                    ObjectValue::file(
                        1,
                        100,
                        Timestamp::default(),
                        Timestamp::default(),
                        Timestamp::default(),
                        Timestamp::default(),
                        None,
                        None,
                    ),
                ),
                Item::new(
                    ObjectKey::attribute(1, AttributeId::TEST_ID, AttributeKey::Attribute),
                    ObjectValue::attribute(100, false),
                ),
                other_object.clone(),
            ],
            &[tombstone, other_object],
        )
        .await;
    }

    #[fuchsia::test]
    async fn test_extent_overlapping_boundaries() {
        use crate::object_store::VOLUME_DATA_KEY_ID;
        let object_id = 1;
        let attr_id = AttributeId::TEST_ID;
        let base = Item::new(
            ObjectKey::extent(object_id, attr_id, 50..100),
            ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID)),
        );

        // 1. Same end, start off-by-one.
        // 49..100 (older, val 2) vs 50..100 (newer, val 1).
        // Yields 49..50 (val 2), 50..100 (val 1).
        let tree = LSMTree::new(merge, None);
        test_merge(
            &tree,
            &[base.clone()],
            &[Item::new(
                ObjectKey::extent(object_id, attr_id, 49..100),
                ObjectValue::Extent(ExtentValue::new_raw(16384, VOLUME_DATA_KEY_ID)),
            )],
            &[
                Item::new(
                    ObjectKey::extent(object_id, attr_id, 49..50),
                    ObjectValue::Extent(ExtentValue::new_raw(16384, VOLUME_DATA_KEY_ID)),
                ),
                base.clone(),
            ],
        )
        .await;

        // 51..100 (older, val 2) vs 50..100 (newer, val 1).
        // Yields 50..100 (val 1).
        let tree = LSMTree::new(merge, None);
        test_merge(
            &tree,
            &[base.clone()],
            &[Item::new(
                ObjectKey::extent(object_id, attr_id, 51..100),
                ObjectValue::Extent(ExtentValue::new_raw(16384, VOLUME_DATA_KEY_ID)),
            )],
            &[base.clone()],
        )
        .await;
    }

    // Tests merging across three LSM tree layers (top, middle, base) with various overlapping
    // combinations. `calculate_expected` constructs a 1-D point oracle where the newest layer
    // (top = 1, middle = 2, base = 3) wins for each byte position, and aggregates continuous
    // segments to verify against the merger iterator output and layer advance depths.
    #[fuchsia::test]
    async fn test_extent_complex_multi_layer() {
        use crate::object_store::extent_record::ExtentValue;
        use crate::object_store::object_record::{
            AttributeKey, ObjectKey, ObjectKeyData, ObjectValue,
        };

        let object_id = 1;
        let attr_id = AttributeId::TEST_ID;

        let top_options = vec![49..50, 50..51, 51..52, 98..99, 99..100, 100..101, 40..95];

        let middle_range = 50..100;

        let base_options = vec![49..101, 49..100, 50..101, 48..102, 100..101, 100..102, 30..90];

        let scale_range =
            |r: std::ops::Range<u64>| r.start * MIN_BLOCK_SIZE..r.end * MIN_BLOCK_SIZE;

        let calculate_expected = |top: std::ops::Range<u64>,
                                  middle: std::ops::Range<u64>,
                                  base: std::ops::Range<u64>| {
            let mut points = vec![0; 200];
            for x in 0u64..200u64 {
                if top.contains(&x) {
                    points[x as usize] = 1;
                } else if middle.contains(&x) {
                    points[x as usize] = 2;
                } else if base.contains(&x) {
                    points[x as usize] = 3;
                }
            }

            let mut result = Vec::new();
            let mut current_val = 0;
            let mut start = 0;
            let mut max_layers_needed = 0;
            let mut start_x = None;

            for x in 0..200 {
                let val = points[x];

                let layers_needed = if top.contains(&(x as u64)) {
                    1
                } else if middle.contains(&(x as u64)) {
                    2
                } else {
                    3
                };

                if val != current_val {
                    if current_val != 0 {
                        result.push((start as u64..x as u64, current_val, 3 - max_layers_needed));
                    }
                    current_val = val;
                    start = x;
                    if start_x.is_none() && val != 0 {
                        start_x = Some(x);
                    }
                }

                if let Some(sx) = start_x {
                    if x >= sx {
                        max_layers_needed = std::cmp::max(max_layers_needed, layers_needed);
                    }
                }
            }
            if current_val != 0 {
                result.push((start as u64..200, current_val, 3 - max_layers_needed));
            }

            result
        };

        for top_range in top_options {
            for base_range in &base_options {
                let expected =
                    calculate_expected(top_range.clone(), middle_range.clone(), base_range.clone());

                let tree = LSMTree::new(merge, None);

                // Base layer (Layer 2)
                tree.insert(Item::new(
                    ObjectKey::extent(object_id, attr_id, scale_range(base_range.clone())),
                    ObjectValue::Extent(ExtentValue::new_raw(0, 3)), // key_id = 3
                ))
                .expect("insert error");
                tree.seal();

                // Middle layer (Layer 1)
                tree.insert(Item::new(
                    ObjectKey::extent(object_id, attr_id, scale_range(middle_range.clone())),
                    ObjectValue::Extent(ExtentValue::new_raw(0, 2)), // key_id = 2
                ))
                .expect("insert error");
                tree.seal();

                // Top layer (Layer 0)
                tree.insert(Item::new(
                    ObjectKey::extent(object_id, attr_id, scale_range(top_range.clone())),
                    ObjectValue::Extent(ExtentValue::new_raw(0, 1)), // key_id = 1
                ))
                .expect("insert error");

                let layer_set = tree.layer_set();

                // Start search with full range of expected results.
                let mut merger = layer_set.merger();
                let mut iter = merger
                    .query(Query::LimitedRange(&ObjectKey::extent(
                        object_id,
                        attr_id,
                        scale_range(expected[0].0.start..expected.last().unwrap().0.end),
                    )))
                    .await
                    .expect("seek failed");

                assert_eq!(iter.pending_iterators_len(), expected[0].2);

                for e in expected {
                    let crate::object_store::ItemRef { key, value, .. } =
                        iter.get().expect("get failed");
                    if let ObjectKeyData::Attribute(aid, AttributeKey::Extent(extent)) = &key.data {
                        assert_eq!(aid, &attr_id);
                        assert_eq!(&**extent, &scale_range(e.0));
                    } else {
                        panic!("Unexpected key type");
                    }
                    if let ObjectValue::Extent(ExtentValue::Some { key_id, .. }) = value {
                        assert_eq!(key_id, &e.1);
                    } else {
                        panic!("Unexpected value type");
                    }
                    assert_eq!(iter.pending_iterators_len(), e.2);
                    iter.advance().await.expect("advance failed");
                }
                assert_eq!(iter.pending_iterators_len(), 0);
                assert!(iter.get().is_none());
            }
        }
    }

    #[fuchsia::test]
    async fn test_next_key_behavior() -> Result<(), Error> {
        let object_id = 1;
        let attr_id = AttributeId::TEST_ID;
        let tree = LSMTree::new(merge, None);

        // Layer 1 (older)
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 50 * MIN_BLOCK_SIZE..101 * MIN_BLOCK_SIZE),
            ObjectValue::Extent(ExtentValue::new_raw(0, VOLUME_DATA_KEY_ID)),
        ))?;
        tree.seal();

        // Layer 0 (newer)
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..100 * MIN_BLOCK_SIZE),
            ObjectValue::Extent(ExtentValue::new_raw(16384, VOLUME_DATA_KEY_ID)),
        ))?;

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();

        let mut iter = merger
            .query(Query::LimitedRange(&ObjectKey::extent(
                object_id,
                attr_id,
                0..100 * MIN_BLOCK_SIZE,
            )))
            .await?;

        let item = iter.get().expect("missing item");
        assert_eq!(item.key, &ObjectKey::extent(object_id, attr_id, 0..100 * MIN_BLOCK_SIZE));

        iter.advance().await.expect("advance failed");

        let item = iter.get().expect("missing item");
        assert_eq!(
            item.key,
            &ObjectKey::extent(object_id, attr_id, 100 * MIN_BLOCK_SIZE..101 * MIN_BLOCK_SIZE,)
        );

        iter.advance().await.expect("advance failed");
        assert!(iter.get().is_none());

        Ok(())
    }

    #[fuchsia::test]
    async fn test_merge_project_usage() {
        let tree = LSMTree::new(merge, None);
        let key = ObjectKey::project_usage(5, ProjectId::new(6).unwrap());

        tree.insert(Item::new(
            key.clone(),
            ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 100, nodes: 1000 }),
        ))
        .expect("insert error");
        tree.merge_into(
            Item::new(
                key.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 4, nodes: 8 }),
            ),
            &key,
        );
        tree.seal();

        tree.merge_into(
            Item::new(
                key.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: -1, nodes: -2 }),
            ),
            &key,
        );
        tree.seal();

        tree.merge_into(
            Item::new(
                key.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 16, nodes: 32 }),
            ),
            &key,
        );

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await.unwrap();
        assert_eq!(iter.get().unwrap().key, &key);
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 119, nodes: 1038 })
        );
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fuchsia::test]
    async fn test_merge_project_usage_gap_layer() {
        let tree = LSMTree::new(merge, None);
        let key = ObjectKey::project_usage(5, ProjectId::new(6).unwrap());
        let key2 = ObjectKey::project_usage(5, ProjectId::new(7).unwrap());

        tree.insert(Item::new(
            key.clone(),
            ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 100, nodes: 1000 }),
        ))
        .expect("insert error");
        tree.merge_into(
            Item::new(
                key.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 4, nodes: 8 }),
            ),
            &key,
        );
        tree.seal();

        assert_eq!(
            tree.find_value(&key).await.expect("Find").unwrap(),
            ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 104, nodes: 1008 })
        );

        tree.merge_into(
            Item::new(
                key2.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 13, nodes: 17 }),
            ),
            &key,
        );
        tree.seal();

        assert_eq!(
            tree.find_value(&key).await.expect("Find").unwrap(),
            ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 104, nodes: 1008 })
        );

        tree.merge_into(
            Item::new(
                key.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 16, nodes: 32 }),
            ),
            &key,
        );

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await.unwrap();
        assert_eq!(iter.get().unwrap().key, &key);
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 120, nodes: 1040 })
        );
        iter.advance().await.unwrap();
        assert_eq!(iter.get().unwrap().key, &key2);
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 13, nodes: 17 })
        );
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());

        assert_eq!(
            tree.find_value(&key).await.expect("Find").unwrap(),
            ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 120, nodes: 1040 })
        );
    }

    #[fuchsia::test]
    async fn test_merge_project_usage_to_zero() {
        let tree = LSMTree::new(merge, None);
        let key = ObjectKey::project_usage(5, ProjectId::new(6).unwrap());

        tree.insert(Item::new(
            key.clone(),
            ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 4, nodes: 8 }),
        ))
        .expect("insert error");
        tree.seal();

        tree.merge_into(
            Item::new(
                key.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: -4, nodes: -8 }),
            ),
            &key,
        );
        tree.seal();

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let iter = merger.query(Query::FullScan).await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fuchsia::test]
    async fn test_merge_project_usage_recover_from_zero() {
        let tree = LSMTree::new(merge, None);
        let key = ObjectKey::project_usage(5, ProjectId::new(6).unwrap());

        tree.insert(Item::new(
            key.clone(),
            ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 4, nodes: 8 }),
        ))
        .expect("insert error");
        tree.seal();

        tree.merge_into(
            Item::new(
                key.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: -4, nodes: -8 }),
            ),
            &key,
        );
        tree.seal();

        tree.merge_into(
            Item::new(
                key.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 20, nodes: 40 }),
            ),
            &key,
        );
        tree.seal();

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await.unwrap();
        assert_eq!(iter.get().unwrap().key, &key);
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 20, nodes: 40 })
        );
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fuchsia::test]
    async fn test_merge_project_usage_layer_merge_to_negative() {
        let tree = LSMTree::new(merge, None);
        let key = ObjectKey::project_usage(5, ProjectId::new(6).unwrap());

        tree.insert(Item::new(
            key.clone(),
            ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 4, nodes: 8 }),
        ))
        .expect("insert error");
        tree.seal();

        tree.merge_into(
            Item::new(
                key.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 16, nodes: 32 }),
            ),
            &key,
        );
        tree.seal();

        // As we merge from the newest layer down this will drop bytes below zero and nodes to
        // exactly zero during the merge process.
        tree.merge_into(
            Item::new(
                key.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: -18, nodes: -32 }),
            ),
            &key,
        );
        tree.seal();

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await.unwrap();
        assert_eq!(iter.get().unwrap().key, &key);
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 2, nodes: 8 })
        );
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fuchsia::test]
    async fn test_merge_posix_owner_usage() {
        let tree = LSMTree::new(merge, None);
        let key = ObjectKey::posix_owner_usage(5, PosixOwner::User(1000));

        tree.insert(Item::new(
            key.clone(),
            ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 100, nodes: 1000 }),
        ))
        .expect("insert error");
        tree.merge_into(
            Item::new(
                key.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 4, nodes: 8 }),
            ),
            &key,
        );
        tree.seal();

        tree.merge_into(
            Item::new(
                key.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: -1, nodes: -2 }),
            ),
            &key,
        );
        tree.seal();

        tree.merge_into(
            Item::new(
                key.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 16, nodes: 32 }),
            ),
            &key,
        );

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await.unwrap();
        assert_eq!(iter.get().unwrap().key, &key);
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 119, nodes: 1038 })
        );
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fuchsia::test]
    async fn test_merge_posix_owner_usage_gap_layer() {
        let tree = LSMTree::new(merge, None);
        let key = ObjectKey::posix_owner_usage(5, PosixOwner::User(1000));
        let key2 = ObjectKey::posix_owner_usage(5, PosixOwner::Group(1000));

        tree.insert(Item::new(
            key.clone(),
            ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 100, nodes: 1000 }),
        ))
        .expect("insert error");
        tree.merge_into(
            Item::new(
                key.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 4, nodes: 8 }),
            ),
            &key,
        );
        tree.merge_into(
            Item::new(
                key2.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 20, nodes: 30 }),
            ),
            &key2,
        );
        tree.seal();

        assert_eq!(
            tree.find_value(&key).await.expect("Find").unwrap(),
            ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 104, nodes: 1008 })
        );
        assert_eq!(
            tree.find_value(&key2).await.expect("Find").unwrap(),
            ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 20, nodes: 30 })
        );

        tree.merge_into(
            Item::new(
                key2.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 13, nodes: 17 }),
            ),
            &key2,
        );
        tree.seal();

        assert_eq!(
            tree.find_value(&key).await.expect("Find").unwrap(),
            ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 104, nodes: 1008 })
        );
        assert_eq!(
            tree.find_value(&key2).await.expect("Find").unwrap(),
            ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 33, nodes: 47 })
        );

        tree.merge_into(
            Item::new(
                key.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 16, nodes: 32 }),
            ),
            &key,
        );

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await.unwrap();
        assert_eq!(iter.get().unwrap().key, &key);
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 120, nodes: 1040 })
        );
        iter.advance().await.unwrap();
        assert_eq!(iter.get().unwrap().key, &key2);
        assert_eq!(
            iter.get().unwrap().value,
            &ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 33, nodes: 47 })
        );
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());

        assert_eq!(
            tree.find_value(&key).await.expect("Find").unwrap(),
            ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 120, nodes: 1040 })
        );
        assert_eq!(
            tree.find_value(&key2).await.expect("Find").unwrap(),
            ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 33, nodes: 47 })
        );
    }

    #[fuchsia::test]
    async fn test_merge_posix_owner_usage_to_zero() {
        let tree = LSMTree::new(merge, None);
        let key = ObjectKey::posix_owner_usage(5, PosixOwner::User(1000));

        tree.insert(Item::new(
            key.clone(),
            ObjectValue::BytesAndNodes(BytesAndNodes { bytes: 4, nodes: 8 }),
        ))
        .expect("insert error");
        tree.seal();

        tree.merge_into(
            Item::new(
                key.clone(),
                ObjectValue::BytesAndNodes(BytesAndNodes { bytes: -4, nodes: -8 }),
            ),
            &key,
        );
        tree.seal();

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let iter = merger.query(Query::FullScan).await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fuchsia::test]
    async fn test_range_query_does_not_reemit_deleted_extents() -> Result<(), Error> {
        let object_id = 1;
        let attr_id = AttributeId::TEST_ID;
        let search_key = ObjectKey::extent(object_id, attr_id, 0..512);

        // Touching deletions across layers queried via Query::FullRange and Query::LimitedRange,
        // while coalescing adjacent deletions within a single mutable layer via `merge_into`.
        let tree = LSMTree::new(merge, None);
        let k2 = ObjectKey::extent(object_id, attr_id, 512..1024);
        tree.merge_into(
            Item::new(k2.clone(), ObjectValue::deleted_extent()),
            &k2.key_for_merge_into(),
        );
        let k3 = ObjectKey::extent(object_id, attr_id, 1024..1536);
        tree.merge_into(
            Item::new(k3.clone(), ObjectValue::deleted_extent()),
            &k3.key_for_merge_into(),
        );
        tree.seal();
        let k1 = ObjectKey::extent(object_id, attr_id, 0..512);
        tree.merge_into(
            Item::new(k1.clone(), ObjectValue::deleted_extent()),
            &k1.key_for_merge_into(),
        );

        let layer_set = tree.layer_set();
        for query in [Query::FullRange(&search_key), Query::LimitedRange(&search_key)] {
            let mut merger = layer_set.merger();
            let mut iter = merger.query(query).await?;
            assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 0..512));
            iter.advance().await?;
            assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, 512..1536));
            iter.advance().await?;
            assert!(iter.get().is_none());
        }

        // Overlapping deletions across layers (both layer orders) must split without re-emitting
        // the already-yielded prefix.
        for (older, newer, expected) in
            [(0..1024, 512..1536, [0..512, 512..1536]), (512..1536, 0..1024, [0..1024, 1024..1536])]
        {
            let tree = LSMTree::new(merge, None);
            tree.insert(Item::new(
                ObjectKey::extent(object_id, attr_id, older),
                ObjectValue::deleted_extent(),
            ))
            .expect("insert error");
            tree.seal();
            tree.insert(Item::new(
                ObjectKey::extent(object_id, attr_id, newer),
                ObjectValue::deleted_extent(),
            ))
            .expect("insert error");

            let layer_set = tree.layer_set();
            for query in [Query::FullRange(&search_key), Query::LimitedRange(&search_key)] {
                let mut merger = layer_set.merger();
                let mut iter = merger.query(query).await?;
                for range in expected.clone() {
                    assert_eq!(
                        iter.get().unwrap().key,
                        &ObjectKey::extent(object_id, attr_id, range)
                    );
                    iter.advance().await?;
                }
                assert!(iter.get().is_none());
            }
        }

        // Three layers of touching deletions.
        let tree = LSMTree::new(merge, None);
        for range in [1024..1536, 512..1024] {
            tree.insert(Item::new(
                ObjectKey::extent(object_id, attr_id, range),
                ObjectValue::deleted_extent(),
            ))
            .expect("insert error");
            tree.seal();
        }
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 0..512),
            ObjectValue::deleted_extent(),
        ))
        .expect("insert error");

        let layer_set = tree.layer_set();
        for query in [Query::FullRange(&search_key), Query::LimitedRange(&search_key)] {
            let mut merger = layer_set.merger();
            let mut iter = merger.query(query).await?;
            for range in [0..512, 512..1024, 1024..1536] {
                assert_eq!(iter.get().unwrap().key, &ObjectKey::extent(object_id, attr_id, range));
                iter.advance().await?;
            }
            assert!(iter.get().is_none());
        }
        Ok(())
    }

    async fn write_persistent_layer_with_num_items(
        items: &[Item<ObjectKey, ObjectValue>],
        num_items: usize,
    ) -> LockedLayer<ObjectKey, ObjectValue> {
        let object = Arc::new(FakeObject::new());
        let write_handle = FakeObjectHandle::new(object.clone());
        let mut writer = PersistentLayerWriter::<_, ObjectKey, ObjectValue>::new(
            Writer::new(&write_handle).await,
            Some(BloomFilterOptions { num_items }),
            BlockSize::SIZE_512B,
        )
        .await
        .expect("PersistentLayerWriter::new failed");
        for item in items {
            writer.write(item.as_item_ref()).await.expect("write failed");
        }
        writer.complete().await.expect("complete failed");
        let layer = PersistentLayer::open(FakeObjectHandle::new(object))
            .await
            .expect("open persistent layer failed");
        LockedLayer::from(layer)
    }

    async fn write_persistent_layer(
        items: Vec<Item<ObjectKey, ObjectValue>>,
    ) -> LockedLayer<ObjectKey, ObjectValue> {
        write_persistent_layer_with_num_items(&items, 1024).await
    }

    #[fuchsia::test]
    async fn test_compaction_coalesces_extents() -> Result<(), Error> {
        let object_id = 1;
        let attr_id = AttributeId::TEST_ID;
        let bucket = BlockSize::SIZE_1MIB.get();
        let empty_bucket_deletion_key =
            ObjectKey::extent(object_id, attr_id, bucket..(bucket + 4096));

        // Base layer kept during minor compaction: contains data in 0..1MiB and 4MiB..6MiB, and
        // nothing in 1MiB..4MiB. Retry if the random per-layer Bloom seed happens to hit a ~0.1%
        // false positive on `empty_bucket_deletion_key`.
        let kept_items = vec![
            Item::new(
                ObjectKey::extent(object_id, attr_id, 0..4096),
                ObjectValue::Extent(ExtentValue::new_raw(65536, VOLUME_DATA_KEY_ID)),
            ),
            Item::new(
                ObjectKey::extent(object_id, attr_id, (4 * bucket)..(5 * bucket + 4096)),
                ObjectValue::Extent(ExtentValue::new_raw(4 * bucket, VOLUME_DATA_KEY_ID)),
            ),
        ];
        let kept_layer = loop {
            let layer = write_persistent_layer(kept_items.clone()).await;
            if layer.maybe_contains_key(&empty_bucket_deletion_key) == MaybeContainsKey::False {
                break layer;
            }
        };

        // Build 3 layers to compact:
        // - Deletions (bucket - 8192)..(bucket - 4096) in L2 and (bucket - 4096)..bucket in L0
        //   are separated by an intermediate layer L1, yet both overlap the kept 0..1MiB bucket
        //   and must coalesce into (bucket - 8192)..bucket.
        // - Touching deletion bucket..(bucket + 4096) in L0 is inserted via `merge_into` (like
        //   `trim_some`): `merge_into` must NOT coalesce it across the 1MiB bucket boundary, so
        //   `minor_iter` elides it via the Bloom filter before `CoalescingIterator` runs.
        // - Contiguous `Raw` and `Overwrite` extents within a 1MiB bucket coalesce across layers,
        //   while non-contiguous `device_offset`, different `key_id`, mixed modes, and `Cow`
        //   extents remain separate.
        let tree = LSMTree::new(merge, None);
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, (bucket - 8192)..(bucket - 4096)),
            ObjectValue::deleted_extent(),
        ))?;
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 16384..20480),
            ObjectValue::Extent(ExtentValue::new_raw(1048576 + 4096, VOLUME_DATA_KEY_ID)),
        ))?;
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 32768..36864),
            ObjectValue::Extent(ExtentValue::initialized_overwrite_extent(
                2097152,
                VOLUME_DATA_KEY_ID,
            )),
        ))?;
        tree.seal();

        // Intermediate layer L1 with an unrelated record so L0 and L2 are non-adjacent.
        tree.insert(Item::new(
            ObjectKey::attribute(object_id, attr_id, AttributeKey::Attribute),
            ObjectValue::attribute(bucket, false),
        ))?;
        tree.seal();

        for range in [(bucket - 4096)..bucket, bucket..(bucket + 4096)] {
            let key = ObjectKey::extent(object_id, attr_id, range);
            let lower_bound = key.key_for_merge_into();
            tree.merge_into(Item::new(key, ObjectValue::deleted_extent()), &lower_bound);
        }
        // Contiguous Raw extent right before 16384..20480.
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 12288..16384),
            ObjectValue::Extent(ExtentValue::new_raw(1048576, VOLUME_DATA_KEY_ID)),
        ))?;
        // Discontiguous Raw extent at 20480..24576 (device_offset jumps).
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 20480..24576),
            ObjectValue::Extent(ExtentValue::new_raw(1048576 + 16384, VOLUME_DATA_KEY_ID)),
        ))?;
        // Contiguous Overwrite extent right after 32768..36864.
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 36864..40960),
            ObjectValue::Extent(ExtentValue::initialized_overwrite_extent(
                2097152 + 4096,
                VOLUME_DATA_KEY_ID,
            )),
        ))?;
        // Contiguous Cow extents at 49152..53248 and 53248..57344 must NOT coalesce.
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 49152..53248),
            ObjectValue::Extent(ExtentValue::with_checksum(
                3145728,
                Checksums::fletcher(vec![1]),
                VOLUME_DATA_KEY_ID,
            )),
        ))?;
        tree.insert(Item::new(
            ObjectKey::extent(object_id, attr_id, 53248..57344),
            ObjectValue::Extent(ExtentValue::with_checksum(
                3145728 + 4096,
                Checksums::fletcher(vec![2]),
                VOLUME_DATA_KEY_ID,
            )),
        ))?;
        // Deletions within 4MiB..5MiB coalesce into one 1MiB deletion, while 5MiB..6MiB stays
        // separate at the 1MiB FuzzyHash bucket boundary.
        for range in [
            (4 * bucket)..(4 * bucket + 4096),
            (4 * bucket + 4096)..(5 * bucket),
            (5 * bucket)..(6 * bucket),
        ] {
            let key = ObjectKey::extent(object_id, attr_id, range);
            let lower_bound = key.key_for_merge_into();
            tree.merge_into(Item::new(key, ObjectValue::deleted_extent()), &lower_bound);
        }

        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut minor = LSMTree::<ObjectKey, ObjectValue>::minor_iter(
            merger.query(Query::FullScan).await?,
            vec![kept_layer],
        )
        .await?;
        let mut minor_items = Vec::new();
        while let Some(item) = minor.get() {
            minor_items.push(item.cloned());
            minor.advance().await?;
        }
        drop(minor);
        assert_eq!(
            minor_items,
            vec![
                Item::new(
                    ObjectKey::attribute(object_id, attr_id, AttributeKey::Attribute),
                    ObjectValue::attribute(bucket, false),
                ),
                Item::new(
                    ObjectKey::extent(object_id, attr_id, 12288..20480),
                    ObjectValue::Extent(ExtentValue::new_raw(1048576, VOLUME_DATA_KEY_ID)),
                ),
                Item::new(
                    ObjectKey::extent(object_id, attr_id, 20480..24576),
                    ObjectValue::Extent(ExtentValue::new_raw(1048576 + 16384, VOLUME_DATA_KEY_ID)),
                ),
                Item::new(
                    ObjectKey::extent(object_id, attr_id, 32768..40960),
                    ObjectValue::Extent(ExtentValue::initialized_overwrite_extent(
                        2097152,
                        VOLUME_DATA_KEY_ID,
                    )),
                ),
                Item::new(
                    ObjectKey::extent(object_id, attr_id, 49152..53248),
                    ObjectValue::Extent(ExtentValue::with_checksum(
                        3145728,
                        Checksums::fletcher(vec![1]),
                        VOLUME_DATA_KEY_ID,
                    )),
                ),
                Item::new(
                    ObjectKey::extent(object_id, attr_id, 53248..57344),
                    ObjectValue::Extent(ExtentValue::with_checksum(
                        3145728 + 4096,
                        Checksums::fletcher(vec![2]),
                        VOLUME_DATA_KEY_ID,
                    )),
                ),
                Item::new(
                    ObjectKey::extent(object_id, attr_id, (bucket - 8192)..bucket),
                    ObjectValue::deleted_extent(),
                ),
                Item::new(
                    ObjectKey::extent(object_id, attr_id, (4 * bucket)..(5 * bucket)),
                    ObjectValue::deleted_extent(),
                ),
                Item::new(
                    ObjectKey::extent(object_id, attr_id, (5 * bucket)..(6 * bucket)),
                    ObjectValue::deleted_extent(),
                ),
            ]
        );

        // Major compaction drops all deleted extents and coalesces the live Raw and Overwrite
        // extents identically.
        let mut major =
            LSMTree::<ObjectKey, ObjectValue>::major_iter(merger.query(Query::FullScan).await?)
                .await?;
        let mut major_items = Vec::new();
        while let Some(item) = major.get() {
            major_items.push(item.cloned());
            major.advance().await?;
        }
        assert_eq!(major_items, minor_items[..minor_items.len() - 3]);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_extent_coalescing_splits_at_bucket_boundaries() -> Result<(), Error> {
        let object_id = 1;
        let attr_id = AttributeId::TEST_ID;
        let bucket = BlockSize::SIZE_1MIB.get();
        let chunk = 64 * 1024;
        let tree = LSMTree::new(merge, None);

        // Merge 4 MiB of 64 KiB contiguous `Raw` writes (`0..4 MiB`) and 4 MiB of 64 KiB
        // contiguous `Overwrite` writes (`4 MiB..8 MiB`). Each 1 MiB `FuzzyHash` bucket should
        // coalesce its 16 chunks into a single 1 MiB extent without crossing bucket boundaries.
        for offset in (0..(4 * bucket)).step_by(chunk as usize) {
            let key = ObjectKey::extent(object_id, attr_id, offset..(offset + chunk));
            let lower_bound = key.key_for_merge_into();
            tree.merge_into(
                Item::new(
                    key,
                    ObjectValue::Extent(ExtentValue::new_raw(bucket + offset, VOLUME_DATA_KEY_ID)),
                ),
                &lower_bound,
            );
        }
        for offset in ((4 * bucket)..(8 * bucket)).step_by(chunk as usize) {
            let key = ObjectKey::extent(object_id, attr_id, offset..(offset + chunk));
            let lower_bound = key.key_for_merge_into();
            tree.merge_into(
                Item::new(
                    key,
                    ObjectValue::Extent(ExtentValue::initialized_overwrite_extent(
                        bucket + offset,
                        VOLUME_DATA_KEY_ID,
                    )),
                ),
                &lower_bound,
            );
        }

        let layer_set = tree.layer_set();
        assert_eq!(layer_set.sum_len(), 8);
        let mut merger = layer_set.merger();
        let mut compacted =
            LSMTree::<ObjectKey, ObjectValue>::major_iter(merger.query(Query::FullScan).await?)
                .await?;
        let mut items = Vec::new();
        while let Some(item) = compacted.get() {
            items.push(item.cloned());
            compacted.advance().await?;
        }
        assert_eq!(items.len(), 8);
        for (i, item) in items.iter().enumerate() {
            let i = i as u64;
            assert_eq!(
                item.key,
                ObjectKey::extent(object_id, attr_id, (i * bucket)..((i + 1) * bucket))
            );
        }

        // Write a persistent layer sized with `layer_set.sum_len()` (just like `tree::flush`).
        // Because every extent spans at most 1 `FuzzyHash` bucket, `maybe_contains_key` checks
        // them normally (never `RangeKeyTooLarge`) and the 8-item Bloom filter is not saturated.
        let layer = write_persistent_layer_with_num_items(&items, layer_set.sum_len()).await;
        for item in &items {
            assert_eq!(layer.maybe_contains_key(&item.key), MaybeContainsKey::Maybe);
        }
        let false_positives = (0..64u64)
            .filter(|i| {
                let key = ObjectKey::extent(999, attr_id, (i * bucket)..((i + 1) * bucket));
                layer.maybe_contains_key(&key) != MaybeContainsKey::False
            })
            .count();
        assert!(false_positives <= 4, "unexpectedly high false positives: {false_positives}/64");
        Ok(())
    }
}
