// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::rcu_intrusive_list::*;
use fuchsia_rcu::RcuReadScope;
use fuchsia_rcu::subtle::RcuPtrRef;

#[derive(Debug, Default)]
struct TestItem {
    value: usize,
    link: Link,
}

impl RcuListAdapter<TestItem> for TestItem {
    rcu_list_adapter!(TestItem, link);
}

#[test]
fn test_intrusive_list_reattach_singleton() {
    let list = RcuIntrusiveList::<TestItem, TestItem>::default();
    let scope = RcuReadScope::new();
    let item = TestItem { value: 42, link: Link::default() };
    let item_ptr = RcuPtrRef::from_ref(&item);

    // Initial attach.
    // SAFETY: Single-threaded test has exclusive writer access to `list`, and `item` is valid,
    // unattached, and outlives `list`.
    unsafe { list.push_back(&scope, item_ptr) };
    assert_eq!(list.iter(&scope).map(|x| x.value).collect::<Vec<_>>(), vec![42]);

    // Detach (poisons prev).
    // SAFETY: Single-threaded test has exclusive writer access to `list`, `item` is attached
    // to `list`, and no concurrent RCU readers exist.
    unsafe { list.remove(&scope, item_ptr) };
    assert!(list.is_empty(&scope));

    // Re-attach singleton: push_back must clear poison from link.prev and ensure link.next
    // is null.
    // SAFETY: Single-threaded test has exclusive writer access to `list`, and `item` is valid,
    // unattached, and outlives `list`.
    unsafe { list.push_back(&scope, item_ptr) };
    assert_eq!(list.iter(&scope).map(|x| x.value).collect::<Vec<_>>(), vec![42]);

    // Subsequent detach must not panic or dereference dangling pointer.
    // SAFETY: Single-threaded test has exclusive writer access to `list`, `item` is attached
    // to `list`, and no concurrent RCU readers exist.
    unsafe { list.remove(&scope, item_ptr) };
    assert!(list.is_empty(&scope));
}

#[test]
fn test_intrusive_list_reattach_middle_no_cycles() {
    let list = RcuIntrusiveList::<TestItem, TestItem>::default();
    let scope = RcuReadScope::new();
    let item1 = TestItem { value: 1, link: Link::default() };
    let item2 = TestItem { value: 2, link: Link::default() };
    let item3 = TestItem { value: 3, link: Link::default() };

    let ptr1 = RcuPtrRef::from_ref(&item1);
    let ptr2 = RcuPtrRef::from_ref(&item2);
    let ptr3 = RcuPtrRef::from_ref(&item3);

    // SAFETY: Single-threaded test has exclusive writer access to `list`, and `item1`, `item2`,
    // and `item3` are valid, unattached, and outlive `list`.
    unsafe {
        list.push_back(&scope, ptr1);
        list.push_back(&scope, ptr2);
        list.push_back(&scope, ptr3);
    }
    assert_eq!(list.iter(&scope).map(|x| x.value).collect::<Vec<_>>(), vec![1, 2, 3]);

    // Detach middle item2.
    // SAFETY: Single-threaded test has exclusive writer access to `list`, `item2` is attached
    // to `list`, and no concurrent RCU readers exist.
    unsafe { list.remove(&scope, ptr2) };
    assert_eq!(list.iter(&scope).map(|x| x.value).collect::<Vec<_>>(), vec![1, 3]);

    // Re-attach item2 to the tail. push_back must null-terminate item2.link.next.
    // SAFETY: Single-threaded test has exclusive writer access to `list`, and `item2` is
    // valid, unattached, and outlives `list`.
    unsafe { list.push_back(&scope, ptr2) };

    // Take 10 to ensure no 2-node cycle exists between item3 and item2.
    let values: Vec<_> = list.iter(&scope).take(10).map(|x| x.value).collect();
    assert_eq!(values, vec![1, 3, 2]);
}

#[test]
fn test_intrusive_list_move_between_lists_no_leakage() {
    let list1 = RcuIntrusiveList::<TestItem, TestItem>::default();
    let list2 = RcuIntrusiveList::<TestItem, TestItem>::default();
    let scope = RcuReadScope::new();

    let item1 = TestItem { value: 10, link: Link::default() };
    let item2 = TestItem { value: 20, link: Link::default() };
    let ptr1 = RcuPtrRef::from_ref(&item1);
    let ptr2 = RcuPtrRef::from_ref(&item2);

    // SAFETY: Single-threaded test has exclusive writer access to `list1`, and `item1` and
    // `item2` are valid, unattached, and outlive `list1`.
    unsafe {
        list1.push_back(&scope, ptr1);
        list1.push_back(&scope, ptr2);
    }
    assert_eq!(list1.iter(&scope).map(|x| x.value).collect::<Vec<_>>(), vec![10, 20]);

    // Move item1 from list1 to list2.
    // SAFETY: Single-threaded test has exclusive writer access to `list1` and `list2`, and
    // `item1` is attached to `list1` prior to removal and outlives both lists.
    unsafe {
        list1.remove(&scope, ptr1);
        list2.push_back(&scope, ptr1);
    }

    // list2 must contain only item1 and must not leak item2 through stale link.next.
    let list1_values: Vec<_> = list1.iter(&scope).take(10).map(|x| x.value).collect();
    let list2_values: Vec<_> = list2.iter(&scope).take(10).map(|x| x.value).collect();

    assert_eq!(list1_values, vec![20]);
    assert_eq!(list2_values, vec![10]);
}

#[test]
fn test_intrusive_list_push_front_reattach() {
    let list1 = RcuIntrusiveList::<TestItem, TestItem>::default();
    let list2 = RcuIntrusiveList::<TestItem, TestItem>::default();
    let scope = RcuReadScope::new();
    let item1 = TestItem { value: 100, link: Link::default() };
    let item2 = TestItem { value: 200, link: Link::default() };
    let ptr1 = RcuPtrRef::from_ref(&item1);
    let ptr2 = RcuPtrRef::from_ref(&item2);

    // SAFETY: Single-threaded test has exclusive writer access to `list1`, and `item1` and
    // `item2` are valid, unattached, and outlive `list1`.
    unsafe {
        list1.push_front(&scope, ptr2);
        list1.push_front(&scope, ptr1);
    }
    assert_eq!(list1.iter(&scope).map(|x| x.value).collect::<Vec<_>>(), vec![100, 200]);

    // Removing `ptr1` from the front of the 2-element list leaves `ptr1.link.next` non-null.
    // SAFETY: Single-threaded test has exclusive writer access to `list1`, `item1` is attached
    // to `list1`, and no concurrent RCU readers exist.
    unsafe { list1.remove(&scope, ptr1) };
    assert_eq!(list1.iter(&scope).map(|x| x.value).collect::<Vec<_>>(), vec![200]);

    // Re-attach via push_front into an empty list: must clear poison and null-terminate `next`.
    // SAFETY: Single-threaded test has exclusive writer access to `list2`, and `item1` is
    // valid, unattached, and outlives `list2`.
    unsafe { list2.push_front(&scope, ptr1) };
    assert_eq!(list2.iter(&scope).take(10).map(|x| x.value).collect::<Vec<_>>(), vec![100]);

    // SAFETY: Single-threaded test has exclusive writer access to `list2`, `item1` is attached
    // to `list2`, and no concurrent RCU readers exist.
    unsafe { list2.remove(&scope, ptr1) };
    assert!(list2.is_empty(&scope));

    // Re-attach via push_front onto a non-empty list (`list1` still contains `ptr2`).
    // SAFETY: Single-threaded test has exclusive writer access to `list1`, and `item1` is
    // valid, unattached, and outlives `list1`.
    unsafe { list1.push_front(&scope, ptr1) };
    assert_eq!(list1.iter(&scope).take(10).map(|x| x.value).collect::<Vec<_>>(), vec![100, 200]);

    // Remove `ptr1` via `RcuIntrusiveListCursor::remove` and re-attach it.
    let mut cursor = list1.cursor(&scope);
    // SAFETY: Single-threaded test has exclusive writer access to `list1`, and no concurrent
    // RCU readers exist.
    let removed = unsafe { cursor.remove() };
    assert_eq!(removed.as_ref().unwrap().value, 100);
    assert_eq!(list1.iter(&scope).map(|x| x.value).collect::<Vec<_>>(), vec![200]);
    // SAFETY: Single-threaded test has exclusive writer access to `list1`, and `removed` is
    // valid, unattached, and outlives `list1`.
    unsafe { list1.push_front(&scope, removed) };
    assert_eq!(list1.iter(&scope).take(10).map(|x| x.value).collect::<Vec<_>>(), vec![100, 200]);

    // Clear `list1` (poisons `prev` on both nodes) and re-attach `ptr1`.
    // SAFETY: Single-threaded test has exclusive writer access to `list1`, and no concurrent
    // RCU readers exist.
    unsafe { list1.clear(&scope, |_| {}) };
    assert!(list1.is_empty(&scope));

    // SAFETY: Single-threaded test has exclusive writer access to `list1`, and `item1` is
    // valid, unattached, and outlives `list1`.
    unsafe { list1.push_front(&scope, ptr1) };
    assert_eq!(list1.iter(&scope).take(10).map(|x| x.value).collect::<Vec<_>>(), vec![100]);
}

#[test]
fn test_intrusive_list_split_off_clears_new_head_prev() {
    let list1 = RcuIntrusiveList::<TestItem, TestItem>::default();
    let scope = RcuReadScope::new();
    let item1 = TestItem { value: 1, link: Link::default() };
    let item2 = TestItem { value: 2, link: Link::default() };
    let item3 = TestItem { value: 3, link: Link::default() };
    let ptr1 = RcuPtrRef::from_ref(&item1);
    let ptr2 = RcuPtrRef::from_ref(&item2);
    let ptr3 = RcuPtrRef::from_ref(&item3);

    // SAFETY: Single-threaded test has exclusive writer access to `list1`, and
    // `item1..=item3` are valid, unattached, and outlive both lists.
    let list2 = unsafe {
        list1.push_back(&scope, ptr1);
        list1.push_back(&scope, ptr2);
        list1.push_back(&scope, ptr3);
        list1.split_off(&scope, 1)
    };
    assert_eq!(list1.iter(&scope).map(|x| x.value).collect::<Vec<_>>(), vec![1]);
    assert_eq!(list2.iter(&scope).map(|x| x.value).collect::<Vec<_>>(), vec![2, 3]);

    // SAFETY: Single-threaded test has exclusive writer access to `list2`, `item2` is
    // attached to `list2`, and no concurrent RCU readers exist.
    unsafe { list2.remove(&scope, ptr2) };
    assert_eq!(list2.iter(&scope).map(|x| x.value).collect::<Vec<_>>(), vec![3]);
    assert_eq!(list1.iter(&scope).map(|x| x.value).collect::<Vec<_>>(), vec![1]);

    // SAFETY: Single-threaded test has exclusive writer access to `list2`, and `item2` is
    // valid, unattached, and outlives `list2`.
    unsafe { list2.push_back(&scope, ptr2) };
    assert_eq!(list2.iter(&scope).map(|x| x.value).collect::<Vec<_>>(), vec![3, 2]);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "Attempted to remove a node whose Link::prev is poisoned")]
fn test_intrusive_list_remove_unattached_panics() {
    let list = RcuIntrusiveList::<TestItem, TestItem>::default();
    let scope = RcuReadScope::new();
    let item = TestItem::default();
    let ptr = RcuPtrRef::from_ref(&item);

    // SAFETY: Intentionally violating the attachment precondition to test debug_assert.
    unsafe { list.remove(&scope, ptr) };
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "Attempted to insert a node that is already attached to a list")]
fn test_intrusive_list_double_insert_panics() {
    let list = RcuIntrusiveList::<TestItem, TestItem>::default();
    let scope = RcuReadScope::new();
    let item = TestItem::default();
    let ptr = RcuPtrRef::from_ref(&item);

    // SAFETY: Intentionally violating the unattached precondition on the second push.
    unsafe {
        list.push_back(&scope, ptr);
        list.push_back(&scope, ptr);
    }
}
