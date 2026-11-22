// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::lsm_tree::merge::ItemOp::{Discard, Keep, Replace};
use crate::lsm_tree::merge::{MergeLayerIterator, MergeResult};
use crate::lsm_tree::types::{Item, LayerIterator};
use crate::object_store::Extent;
use crate::object_store::allocator::{AllocatorKey, AllocatorValue};
use anyhow::Error;
use std::collections::HashSet;

pub fn merge(
    left: &MergeLayerIterator<'_, AllocatorKey, AllocatorValue>,
    right: &MergeLayerIterator<'_, AllocatorKey, AllocatorValue>,
) -> MergeResult<AllocatorKey, AllocatorValue> {
    // Wherever Replace is used below, it must not extend the *end* of the range for whichever item
    // is returned i.e. if replacing the left item, replacement.end <= left.end because otherwise we
    // might not merge records that come after that end point because the merger won't merge records
    // in the same layer

    /*  Case 1: Disjoint or touching
     *    L:    |------------|
     *    R:                      |-----------|
     *  or
     *    L:    |------------|
     *    R:                 |-----------|
     *
     *  Single-layer `merge_into` insertions and post-merge streams coalesce touching records via
     *  `Coalesce` / `CoalescingIterator` rather than inside `MergeFn`.
     */
    if left.key().device_range.end <= right.key().device_range.start {
        return MergeResult::EmitLeft;
    }

    if left.key().device_range.start == right.key().device_range.start {
        /*  Case 2: Overlap with same start
         *    L:    |------------|
         *    R:    |-----------------|
         */
        if left.key().device_range.end < right.key().device_range.end {
            // The newer value eclipses the older.
            if left.layer_index < right.layer_index {
                return MergeResult::Other {
                    emit: None,
                    left: Keep,
                    right: if left.key().device_range.end == right.key().device_range.end {
                        Discard
                    } else {
                        Replace(
                            Item::new(
                                AllocatorKey {
                                    device_range: Extent(
                                        left.key().device_range.end..right.key().device_range.end,
                                    ),
                                },
                                right.value().clone(),
                            )
                            .boxed(),
                        )
                    },
                };
            } else {
                // right is a newer Abs/None than left
                return MergeResult::Other { emit: None, left: Discard, right: Keep };
            }

        /*  Case 3: Overlap with same start
         *    L:    |-----------------|
         *    R:    |------------|
         */
        } else {
            // The newer value eclipses the older.
            if right.layer_index < left.layer_index {
                return MergeResult::Other {
                    emit: None,
                    left: if right.key().device_range.end == left.key().device_range.end {
                        Discard
                    } else {
                        Replace(
                            Item::new(
                                AllocatorKey {
                                    device_range: Extent(
                                        right.key().device_range.end..left.key().device_range.end,
                                    ),
                                },
                                left.value().clone(),
                            )
                            .boxed(),
                        )
                    },
                    right: Keep,
                };
            } else {
                // right is a newer Abs/None than left
                return MergeResult::Other { emit: None, left: Keep, right: Discard };
            }
        }
    }
    /*  Case 4: Split off left prefix
     *    L:    |-----...
     *    R:         |-----...
     */
    debug_assert!(left.key().device_range.end >= right.key().device_range.start);
    MergeResult::Other {
        emit: Some(
            Item::new(
                AllocatorKey {
                    device_range: Extent(
                        left.key().device_range.start..right.key().device_range.start,
                    ),
                },
                left.value().clone(),
            )
            .boxed(),
        ),
        left: Replace(
            Item::new(
                AllocatorKey {
                    device_range: Extent(
                        right.key().device_range.start..left.key().device_range.end,
                    ),
                },
                left.value().clone(),
            )
            .boxed(),
        ),
        right: Keep,
    }
}

pub fn filter_tombstones<'a>(
    iter: impl LayerIterator<AllocatorKey, AllocatorValue> + 'a,
) -> impl Future<Output = Result<impl LayerIterator<AllocatorKey, AllocatorValue> + 'a, Error>> {
    iter.filter(|i| *i.value != AllocatorValue::None)
}

pub fn filter_marked_for_deletion<'a>(
    iter: impl LayerIterator<AllocatorKey, AllocatorValue> + 'a,
    marked_for_deletion: HashSet<u64>,
) -> impl Future<Output = Result<impl LayerIterator<AllocatorKey, AllocatorValue> + 'a, Error>> {
    iter.filter(move |i| {
        if let AllocatorValue::Abs { owner_object_id, .. } = i.value {
            !marked_for_deletion.contains(owner_object_id)
        } else {
            true
        }
    })
}

#[cfg(test)]
mod tests {
    use crate::lsm_tree::types::{Item, ItemRef, LayerIterator};
    use crate::lsm_tree::{LSMTree, Query};
    use crate::object_store::allocator::merge::{filter_tombstones, merge};
    use crate::object_store::allocator::{AllocatorKey, AllocatorValue};
    use std::ops::Range;

    // Tests merge logic given (range, delta and object_id) for left, right and expected output.
    async fn test_merge(
        left: (Range<u64>, AllocatorValue),
        right: (Range<u64>, AllocatorValue),
        expected: &[(Range<u64>, AllocatorValue)],
    ) {
        let tree = LSMTree::new(merge, None);
        tree.insert(Item::new(AllocatorKey { device_range: right.0.into() }, right.1))
            .expect("insert error");
        tree.seal();
        tree.insert(Item::new(AllocatorKey { device_range: left.0.into() }, left.1))
            .expect("insert error");
        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = filter_tombstones(merger.query(Query::FullScan).await.expect("seek failed"))
            .await
            .expect("filter failed");
        for e in expected {
            let ItemRef { key, value, .. } = iter.get().expect("get failed");
            assert_eq!((key, value), (&AllocatorKey { device_range: e.0.clone().into() }, &e.1));
            iter.advance().await.expect("advance failed");
        }
        assert!(iter.get().is_none());
    }

    #[fuchsia::test]
    async fn test_no_overlap() {
        test_merge(
            (0..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            (200..300, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            &[
                (0..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
                (200..300, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            ],
        )
        .await;
    }

    #[fuchsia::test]
    async fn test_touching() {
        test_merge(
            (0..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            (100..200, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            &[
                (0..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
                (100..200, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            ],
        )
        .await;

        let val = AllocatorValue::Abs { count: 1, owner_object_id: 1 };
        let tree = LSMTree::new(merge, None);
        tree.insert(Item::new(AllocatorKey { device_range: (100..200).into() }, val.clone()))
            .expect("insert error");
        tree.seal();
        tree.insert(Item::new(AllocatorKey { device_range: (0..100).into() }, val.clone()))
            .expect("insert error");
        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger
            .query(Query::FullScan)
            .await
            .expect("seek failed")
            .coalesce()
            .await
            .expect("coalesce failed");
        assert_eq!(
            iter.get(),
            Some(ItemRef { key: &AllocatorKey { device_range: (0..200).into() }, value: &val })
        );
        iter.advance().await.expect("advance failed");
        assert!(iter.get().is_none());
    }

    #[fuchsia::test]
    async fn test_identical() {
        test_merge(
            (0..100, AllocatorValue::Abs { count: 2, owner_object_id: 1 }),
            (0..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            &[(0..100, AllocatorValue::Abs { count: 2, owner_object_id: 1 })],
        )
        .await;
        test_merge(
            (0..100, AllocatorValue::None),
            (0..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            &[],
        )
        .await;
    }

    #[fuchsia::test]
    async fn test_left_smaller_than_right_with_same_start() {
        test_merge(
            (0..100, AllocatorValue::Abs { count: 2, owner_object_id: 1 }),
            (0..200, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            &[
                (0..100, AllocatorValue::Abs { count: 2, owner_object_id: 1 }),
                (100..200, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            ],
        )
        .await;
        test_merge(
            (0..100, AllocatorValue::None),
            (0..200, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            &[(100..200, AllocatorValue::Abs { count: 1, owner_object_id: 1 })],
        )
        .await;
    }

    #[fuchsia::test]
    async fn test_left_starts_before_right_with_overlap() {
        test_merge(
            (0..200, AllocatorValue::Abs { count: 2, owner_object_id: 1 }),
            (100..150, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            &[
                (0..100, AllocatorValue::Abs { count: 2, owner_object_id: 1 }),
                (100..200, AllocatorValue::Abs { count: 2, owner_object_id: 1 }),
            ],
        )
        .await;
    }

    #[fuchsia::test]
    async fn test_different_object_id() {
        // Case 1
        test_merge(
            (0..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            (200..300, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            &[
                (0..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
                (200..300, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            ],
        )
        .await;
        // Case 2
        test_merge(
            (0..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            (100..200, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            &[
                (0..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
                (100..200, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            ],
        )
        .await;
        // Case 3
        test_merge(
            (0..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            (0..100, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            &[(0..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 })],
        )
        .await;
        // Case 4
        test_merge(
            (0..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            (0..200, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            &[
                (0..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
                (100..200, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            ],
        )
        .await;
        // Case 5
        test_merge(
            (0..200, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            (0..100, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            &[(0..200, AllocatorValue::Abs { count: 1, owner_object_id: 1 })],
        )
        .await;
        // Case 6
        test_merge(
            (0..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            (50..150, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            &[
                (0..50, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
                (50..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
                (100..150, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            ],
        )
        .await;
    }

    #[fuchsia::test]
    async fn test_tombstones() {
        // We have to make sure we don't prematurely discard records. seal() may be called at
        // any time and the resulting layer tree must remain valid.
        // Here we test absolute allocation counts and reuse of allocated space.
        //
        //  1. Alloc object_id A, write layer file.
        //  2. Dealloc object_id A, Alloc object_id B, write layer file.
        //  3. Dealloc object_id B, Alloc object_id A.
        let key = AllocatorKey { device_range: (0..100 * 512).into() };
        let lower_bound = AllocatorKey::lower_bound_for_merge_into(&key);
        let tree = LSMTree::new(merge, None);
        tree.merge_into(
            Item::new(key.clone(), AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            &lower_bound,
        );
        tree.seal();
        tree.merge_into(
            Item::new(key.clone(), AllocatorValue::Abs { count: 2, owner_object_id: 1 }),
            &lower_bound,
        );
        tree.seal();
        tree.merge_into(Item::new(key.clone(), AllocatorValue::None), &lower_bound);
        tree.merge_into(
            Item::new(key.clone(), AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            &lower_bound,
        );
        tree.seal();
        tree.merge_into(Item::new(key.clone(), AllocatorValue::None), &lower_bound);
        tree.merge_into(
            Item::new(key.clone(), AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            &lower_bound,
        );
        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let mut iter = merger.query(Query::FullScan).await.expect("seek failed");
        let ItemRef { key: k, value, .. } = iter.get().expect("get failed");
        assert_eq!((k, value), (&key, &AllocatorValue::Abs { count: 1, owner_object_id: 1 }));
        iter.advance().await.expect("advance failed");
        assert!(iter.get().is_none());
    }

    #[fuchsia::test]
    async fn test_merge_adjacent_in_mutable_layer() {
        // Left-to-right insertion
        {
            let tree = LSMTree::new(merge, None);

            let key1 = AllocatorKey { device_range: (4096..135168).into() };
            let val1 = AllocatorValue::Abs { count: 1, owner_object_id: 3 };
            tree.merge_into(
                Item::new(key1.clone(), val1.clone()),
                &key1.lower_bound_for_merge_into(),
            );

            let key2 = AllocatorKey { device_range: (135168..139264).into() };
            let val2 = AllocatorValue::Abs { count: 1, owner_object_id: 3 };
            tree.merge_into(
                Item::new(key2.clone(), val2.clone()),
                &key2.lower_bound_for_merge_into(),
            );

            // They should be merged into 4096..139264 in the mutable layer.
            let layer_set = tree.layer_set();
            let mut merger = layer_set.merger();
            let mut iter = merger.query(Query::FullScan).await.expect("seek failed");

            let ItemRef { key, value, .. } = iter.get().expect("get failed");
            assert_eq!(
                (key, value),
                (
                    &AllocatorKey { device_range: (4096..139264).into() },
                    &AllocatorValue::Abs { count: 1, owner_object_id: 3 }
                )
            );
            iter.advance().await.expect("advance failed");
            assert!(iter.get().is_none());
        }

        // Right-to-left (reverse) insertion
        {
            let tree = LSMTree::new(merge, None);

            let key2 = AllocatorKey { device_range: (135168..139264).into() };
            let val2 = AllocatorValue::Abs { count: 1, owner_object_id: 3 };
            tree.merge_into(
                Item::new(key2.clone(), val2.clone()),
                &key2.lower_bound_for_merge_into(),
            );

            let key1 = AllocatorKey { device_range: (4096..135168).into() };
            let val1 = AllocatorValue::Abs { count: 1, owner_object_id: 3 };
            tree.merge_into(
                Item::new(key1.clone(), val1.clone()),
                &key1.lower_bound_for_merge_into(),
            );

            // They should be merged into 4096..139264 in the mutable layer.
            let layer_set = tree.layer_set();
            let mut merger = layer_set.merger();
            let mut iter = merger.query(Query::FullScan).await.expect("seek failed");

            let ItemRef { key, value, .. } = iter.get().expect("get failed");
            assert_eq!(
                (key, value),
                (
                    &AllocatorKey { device_range: (4096..139264).into() },
                    &AllocatorValue::Abs { count: 1, owner_object_id: 3 }
                )
            );
            iter.advance().await.expect("advance failed");
            assert!(iter.get().is_none());
        }

        // Three-way hole fill (`0..100` and `200..300`, then `100..200`) coalesces into `0..300`.
        {
            let tree = LSMTree::new(merge, None);
            let val = AllocatorValue::Abs { count: 1, owner_object_id: 3 };
            for range in [0..100, 200..300, 100..200] {
                let key = AllocatorKey { device_range: range.into() };
                tree.merge_into(
                    Item::new(key.clone(), val.clone()),
                    &key.lower_bound_for_merge_into(),
                );
            }
            assert_eq!(tree.mutable_layer().len(), 1);
            let layer_set = tree.layer_set();
            let mut merger = layer_set.merger();
            let mut iter = merger.query(Query::FullScan).await.expect("seek failed");
            assert_eq!(
                iter.get(),
                Some(ItemRef { key: &AllocatorKey { device_range: (0..300).into() }, value: &val })
            );
            iter.advance().await.expect("advance failed");
            assert!(iter.get().is_none());
        }
    }

    #[fuchsia::test]
    async fn test_overlapping_boundaries() {
        let base = (50..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 });

        let test_val = AllocatorValue::Abs { count: 1, owner_object_id: 2 };

        // 1. Same end, start off-by-one.
        // 49..100 vs 50..100

        // Test with base (50..100) newer
        test_merge(
            base.clone(),
            (49..100, test_val.clone()),
            &[
                (49..50, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
                (50..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            ],
        )
        .await;
        // Test with test_range (49..100) newer
        test_merge(
            (49..100, test_val.clone()),
            base.clone(),
            &[
                (49..50, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
                (50..100, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            ],
        )
        .await;

        // 51..100 vs 50..100
        // Test with base (50..100) newer
        test_merge(
            base.clone(),
            (51..100, test_val.clone()),
            &[
                (50..51, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
                (51..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            ],
        )
        .await;
        // Test with test_range (51..100) newer
        test_merge(
            (51..100, test_val.clone()),
            base.clone(),
            &[
                (50..51, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
                (51..100, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            ],
        )
        .await;

        // 2. End off-by-one, same start.
        // 50..99 vs 50..100
        // Test with base (50..100) newer
        test_merge(
            base.clone(),
            (50..99, test_val.clone()),
            &[(50..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 })],
        )
        .await;
        // Test with test_range (50..99) newer
        test_merge(
            (50..99, test_val.clone()),
            base.clone(),
            &[
                (50..99, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
                (99..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            ],
        )
        .await;

        // 50..101 vs 50..100
        // Test with base (50..100) newer
        test_merge(
            base.clone(),
            (50..101, test_val.clone()),
            &[
                (50..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
                (100..101, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            ],
        )
        .await;
        // Test with test_range (50..101) newer
        test_merge(
            (50..101, test_val.clone()),
            base.clone(),
            &[(50..101, AllocatorValue::Abs { count: 1, owner_object_id: 2 })],
        )
        .await;

        // 3. Both off-by-one.
        // 49..99 vs 50..100
        // Test with base (50..100) newer
        test_merge(
            base.clone(),
            (49..99, test_val.clone()),
            &[
                (49..50, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
                (50..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            ],
        )
        .await;
        // Test with test_range (49..99) newer
        test_merge(
            (49..99, test_val.clone()),
            base.clone(),
            &[
                (49..50, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
                (50..99, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
                (99..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            ],
        )
        .await;

        // 51..101 vs 50..100
        // Test with base (50..100) newer
        test_merge(
            base.clone(),
            (51..101, test_val.clone()),
            &[
                (50..51, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
                (51..100, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
                (100..101, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            ],
        )
        .await;
        // Test with test_range (51..101) newer
        test_merge(
            (51..101, test_val.clone()),
            base.clone(),
            &[
                (50..51, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
                (51..101, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            ],
        )
        .await;
    }

    #[fuchsia::test]
    async fn test_length_1_ranges() {
        // Touching length 1 ranges.
        test_merge(
            (10..11, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            (11..12, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            &[
                (10..11, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
                (11..12, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            ],
        )
        .await;

        // Identical length 1 ranges.
        test_merge(
            (10..11, AllocatorValue::Abs { count: 1, owner_object_id: 1 }),
            (10..11, AllocatorValue::Abs { count: 1, owner_object_id: 2 }),
            &[(10..11, AllocatorValue::Abs { count: 1, owner_object_id: 1 })],
        )
        .await;
    }

    #[fuchsia::test]
    async fn test_full_range_query_does_not_reemit_coalesced_extents() {
        let val = AllocatorValue::Abs { count: 1, owner_object_id: 1 };

        // Touching extents across layers queried via Query::FullRange and Query::LimitedRange
        // (OptimizedMerge), while still coalescing within a single mutable layer via `merge_into`.
        let tree = LSMTree::new(merge, None);
        let k2 = AllocatorKey { device_range: (512..1024).into() };
        tree.merge_into(Item::new(k2.clone(), val.clone()), &k2.lower_bound_for_merge_into());
        let k3 = AllocatorKey { device_range: (1024..1536).into() };
        tree.merge_into(Item::new(k3.clone(), val.clone()), &k3.lower_bound_for_merge_into());
        tree.seal();
        let k1 = AllocatorKey { device_range: (0..512).into() };
        tree.merge_into(Item::new(k1.clone(), val.clone()), &k1.lower_bound_for_merge_into());

        let layer_set = tree.layer_set();
        let search_key = AllocatorKey { device_range: (0..512).into() };
        for query in [Query::FullRange(&search_key), Query::LimitedRange(&search_key)] {
            let mut merger = layer_set.merger();
            let mut iter = merger.query(query).await.expect("seek failed");
            assert_eq!(iter.get().unwrap().key, &AllocatorKey { device_range: (0..512).into() });
            iter.advance().await.expect("advance failed");
            // 512..1024 and 1024..1536 were coalesced into 512..1536 within the older mutable
            // layer via `merge_into`, and must not coalesce across layers with 0..512.
            assert_eq!(iter.get().unwrap().key, &AllocatorKey { device_range: (512..1536).into() });
            iter.advance().await.expect("advance failed");
            assert!(iter.get().is_none());
        }

        // Overlapping extents that split into touching extents during advance.
        let tree = LSMTree::new(merge, None);
        tree.insert(Item::new(AllocatorKey { device_range: (4096..12288).into() }, val.clone()))
            .expect("insert error");
        tree.seal();
        tree.insert(Item::new(AllocatorKey { device_range: (0..8192).into() }, val.clone()))
            .expect("insert error");

        let layer_set = tree.layer_set();
        for query in [Query::FullRange(&search_key), Query::LimitedRange(&search_key)] {
            let mut merger = layer_set.merger();
            let mut iter = merger.query(query).await.expect("seek failed");
            assert_eq!(iter.get().unwrap().key, &AllocatorKey { device_range: (0..8192).into() });
            iter.advance().await.expect("advance failed");
            assert_eq!(
                iter.get().unwrap().key,
                &AllocatorKey { device_range: (8192..12288).into() }
            );
            iter.advance().await.expect("advance failed");
            assert!(iter.get().is_none());
        }

        // Three layers of touching extents: each extent is yielded once and never coalesced with
        // an extent that has already been yielded.
        let tree = LSMTree::new(merge, None);
        tree.insert(Item::new(AllocatorKey { device_range: (1024..1536).into() }, val.clone()))
            .expect("insert error");
        tree.seal();
        tree.insert(Item::new(AllocatorKey { device_range: (512..1024).into() }, val.clone()))
            .expect("insert error");
        tree.seal();
        tree.insert(Item::new(AllocatorKey { device_range: (0..512).into() }, val))
            .expect("insert error");

        let layer_set = tree.layer_set();
        for query in [Query::FullRange(&search_key), Query::LimitedRange(&search_key)] {
            let mut merger = layer_set.merger();
            let mut iter = merger.query(query).await.expect("seek failed");
            for range in [0..512, 512..1024, 1024..1536] {
                assert_eq!(iter.get().unwrap().key, &AllocatorKey { device_range: range.into() });
                iter.advance().await.expect("advance failed");
            }
            assert!(iter.get().is_none());
        }
    }
}
