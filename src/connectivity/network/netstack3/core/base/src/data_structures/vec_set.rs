// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! An ordered set backed by a sorted [`Vec`].

use alloc::vec::Vec;

/// An ordered set backed by a sorted, deduplicated [`Vec<T>`].
///
/// `VecSet` maintains its elements contiguously in strictly ascending [`Ord`]
/// order. Lookups use binary search (`O(log n)` comparisons), while insertions
/// and removals shift trailing elements in place (`O(n)` moves).
///
/// # When to use `VecSet` instead of `BTreeSet`
///
/// `VecSet` exists for ordered sets that are expected to stay small in normal
/// operation, such as tracking missing fragment-block ranges during IP packet
/// reassembly, where in-order arrival maintains a single trailing gap and
/// out-of-order arrival typically creates only a handful of gaps:
///
/// - **Cache locality and low overhead for small `n`:** All elements live in a
///   single contiguous buffer with zero per-element node or pointer overhead
///   (and zero heap allocations when empty). For small sets, especially of
///   small, trivially movable types, binary search plus a short `memmove`
///   avoids pointer-chasing across B-tree nodes as well as node splits,
///   merges, and rebalancing.
/// - **Smaller binary size:** [`alloc::collections::BTreeSet`] pulls in
///   substantial B-tree node management code (and, via `FromIterator`, slice
///   sorting). `VecSet` reuses [`Vec`] and [`slice::binary_search`], keeping
///   monomorphized code tiny and allowing resource-constrained `no_std` builds
///   with no other B-tree users to dead-strip `alloc::collections::btree`
///   entirely.
///
/// Prefer [`alloc::collections::BTreeSet`] when the set can grow large and
/// undergoes frequent insertions or removals away from the end (where shifting
/// elements makes updates `O(n)` instead of `O(log n)`), or `HashSet` when
/// element ordering is not required.
///
/// # Ordering invariant
///
/// Like [`alloc::collections::BTreeSet`], it is a logic error for an element to
/// be modified through interior mutability in a way that changes its [`Ord`]
/// ordering relative to any other element while it is stored in the set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VecSet<T>(Vec<T>);

impl<T> Default for VecSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> VecSet<T> {
    /// Creates an empty `VecSet` without allocating.
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// Creates a `VecSet` containing the single element `value`.
    pub fn with_single(value: T) -> Self {
        Self(alloc::vec![value])
    }

    /// Returns `true` if the set contains no elements.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns an iterator that visits the elements in ascending order.
    pub fn iter(&self) -> core::slice::Iter<'_, T> {
        self.0.iter()
    }
}

impl<T: Ord> VecSet<T> {
    /// Adds a value to the set.
    ///
    /// Returns whether the value was newly inserted:
    /// - `true` if the set did not previously contain an equal value.
    /// - `false` if an equal value was already present, leaving the existing
    ///   element in place without updating it.
    pub fn insert(&mut self, value: T) -> bool {
        match self.0.binary_search(&value) {
            Ok(_) => false,
            Err(index) => {
                self.0.insert(index, value);
                true
            }
        }
    }

    /// Removes an element equal to `value` from the set, returning whether such
    /// an element was present.
    pub fn remove(&mut self, value: &T) -> bool {
        match self.0.binary_search(value) {
            Ok(index) => {
                let _: T = self.0.remove(index);
                true
            }
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::collections::BTreeSet;
    use core::cmp::Ordering;

    use proptest::prop_assert_eq;
    use proptest::strategy::Strategy;

    // Intentionally omits `Default` and `Ord` to verify unconditional impls on
    // `VecSet<T>`.
    #[derive(Debug, Eq, PartialEq)]
    struct UnorderedNoDefault(u8);

    #[test]
    fn empty_and_single() {
        let empty: VecSet<UnorderedNoDefault> = VecSet::default();
        assert!(empty.is_empty());
        assert_eq!(empty.iter().next(), None);

        let mut single = VecSet::with_single(42u16);
        assert!(!single.is_empty());
        assert_eq!(single.iter().copied().collect::<Vec<_>>(), alloc::vec![42]);
        assert!(!single.insert(42));
        assert!(!single.remove(&7));
        assert!(single.remove(&42));
        assert!(single.is_empty());
        assert_eq!(single.iter().next(), None);
    }

    #[test]
    fn insert_and_remove_sorted_order() {
        let mut set = VecSet::new();

        // Insert elements out of order, including new minimum, middle, and
        // maximum values, plus duplicates.
        assert!(set.insert(16u16));
        assert!(set.insert(0));
        assert!(set.insert(8));
        assert!(set.insert(4));
        assert!(!set.insert(8));
        assert!(!set.insert(0));
        assert!(!set.insert(16));

        assert_eq!(set.iter().copied().collect::<Vec<_>>(), alloc::vec![0, 4, 8, 16]);

        // Remove absent, middle, first, and last elements in sequence.
        assert!(!set.remove(&6));
        assert!(set.remove(&4));
        assert_eq!(set.iter().copied().collect::<Vec<_>>(), alloc::vec![0, 8, 16]);
        assert!(set.remove(&0));
        assert_eq!(set.iter().copied().collect::<Vec<_>>(), alloc::vec![8, 16]);
        assert!(set.remove(&16));
        assert_eq!(set.iter().copied().collect::<Vec<_>>(), alloc::vec![8]);
        assert!(set.remove(&8));
        assert!(set.is_empty());
    }

    #[derive(Clone, Debug)]
    struct KeyedItem {
        key: u16,
        tag: &'static str,
    }

    impl PartialEq for KeyedItem {
        fn eq(&self, other: &Self) -> bool {
            self.key == other.key
        }
    }

    impl Eq for KeyedItem {}

    impl PartialOrd for KeyedItem {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    impl Ord for KeyedItem {
        fn cmp(&self, other: &Self) -> Ordering {
            self.key.cmp(&other.key)
        }
    }

    #[test]
    fn duplicate_insert_preserves_existing_element() {
        let mut set = VecSet::new();
        assert!(set.insert(KeyedItem { key: 2, tag: "first" }));
        assert!(set.insert(KeyedItem { key: 1, tag: "one" }));
        assert!(!set.insert(KeyedItem { key: 2, tag: "second" }));

        let tags: Vec<_> = set.iter().map(|item| (item.key, item.tag)).collect();
        assert_eq!(tags, alloc::vec![(1, "one"), (2, "first")]);

        assert!(set.remove(&KeyedItem { key: 2, tag: "ignored" }));
        let tags: Vec<_> = set.iter().map(|item| (item.key, item.tag)).collect();
        assert_eq!(tags, alloc::vec![(1, "one")]);
    }

    #[derive(Clone, Debug)]
    enum Operation {
        Insert(u8),
        Remove(u8),
    }

    fn operation_strategy() -> impl Strategy<Value = Operation> {
        let value = 0..16u8;
        proptest::prop_oneof![
            value.clone().prop_map(Operation::Insert),
            value.prop_map(Operation::Remove),
        ]
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            failure_persistence: proptest_support::failed_seeds_no_std!(),
            ..proptest::test_runner::Config::default()
        })]

        #[test]
        fn arbitrary_operations_match_btreeset(
            operations in proptest::collection::vec(operation_strategy(), 0..64),
        ) {
            let mut set = VecSet::new();
            let mut reference = BTreeSet::new();

            for operation in operations {
                match operation {
                    Operation::Insert(value) => {
                        prop_assert_eq!(set.insert(value), reference.insert(value));
                    }
                    Operation::Remove(value) => {
                        prop_assert_eq!(set.remove(&value), reference.remove(&value));
                    }
                }

                prop_assert_eq!(set.is_empty(), reference.is_empty());
                let items: Vec<u8> = set.iter().copied().collect();
                let expected: Vec<u8> = reference.iter().copied().collect();
                prop_assert_eq!(items, expected);
            }
        }
    }
}
