// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::rcu_intrusive_list::{Link, RcuListAdapter, rcu_list_adapter};
use crate::rcu_list::*;
use fuchsia_rcu::{RcuDroppable, RcuReadScope, rcu_run_callbacks};

#[derive(Debug)]
struct TestNode {
    value: i64,
    link: Link,
}

// SAFETY: TestNode does not perform any blocking or contextual work on drop.
unsafe impl RcuDroppable for TestNode {}

impl TestNode {
    fn new(value: i64) -> Self {
        Self { value, link: Default::default() }
    }
}

impl RcuListAdapter<TestNode> for TestNode {
    rcu_list_adapter!(TestNode, link);
}

#[test]
fn test_rcu_list_push_front() {
    {
        let list = RcuList::<TestNode, TestNode>::default();
        let scope = RcuReadScope::new();
        unsafe {
            list.push_front(&scope, TestNode::new(1));
            list.push_front(&scope, TestNode::new(2));
            list.push_front(&scope, TestNode::new(3));
        }

        let mut cursor = list.cursor(&scope);
        assert_eq!(cursor.current().map(|node| node.value), Some(3));
        cursor.advance();
        assert_eq!(cursor.current().map(|node| node.value), Some(2));
        cursor.advance();
        assert_eq!(cursor.current().map(|node| node.value), Some(1));
        cursor.advance();
        assert_eq!(cursor.current().map(|node| node.value), None);
    }
    rcu_run_callbacks();
}

#[test]
fn test_rcu_list_push_back() {
    {
        let list = RcuList::<TestNode, TestNode>::default();
        let scope = RcuReadScope::new();
        unsafe {
            list.push_back(&scope, TestNode::new(1));
            list.push_back(&scope, TestNode::new(2));
            list.push_back(&scope, TestNode::new(3));
        }

        let mut cursor = list.cursor(&scope);
        assert_eq!(cursor.current().map(|node| node.value), Some(1));
        cursor.advance();
        assert_eq!(cursor.current().map(|node| node.value), Some(2));
        cursor.advance();
        assert_eq!(cursor.current().map(|node| node.value), Some(3));
        cursor.advance();
        assert_eq!(cursor.current().map(|node| node.value), None);
    }
    rcu_run_callbacks();
}

#[test]
fn test_rcu_list_clear() {
    {
        let list = RcuList::<TestNode, TestNode>::default();
        let scope = RcuReadScope::new();
        unsafe {
            list.push_back(&scope, TestNode::new(1));
            list.push_back(&scope, TestNode::new(2));
            list.push_back(&scope, TestNode::new(3));
        }

        unsafe { list.clear() };

        let mut iter = list.iter(&scope);
        assert_eq!(iter.next().map(|node| node.value), None);
    }

    rcu_run_callbacks();
}

#[test]
fn test_rcu_list_drop_clears_objects() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct DropCounter {
        _id: usize,
        counter: Arc<AtomicUsize>,
        link: Link,
    }

    // SAFETY: DropCounter only increments an atomic counter on drop.
    unsafe impl RcuDroppable for DropCounter {}

    impl RcuListAdapter<DropCounter> for DropCounter {
        rcu_list_adapter!(DropCounter, link);
    }

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.counter.fetch_add(1, Ordering::SeqCst);
        }
    }

    let drop_count = Arc::new(AtomicUsize::new(0));
    {
        let list = RcuList::<DropCounter, DropCounter>::default();
        let scope = RcuReadScope::new();
        unsafe {
            list.push_back(
                &scope,
                DropCounter { _id: 1, counter: Arc::clone(&drop_count), link: Default::default() },
            );
            list.push_back(
                &scope,
                DropCounter { _id: 2, counter: Arc::clone(&drop_count), link: Default::default() },
            );
            list.push_back(
                &scope,
                DropCounter { _id: 3, counter: Arc::clone(&drop_count), link: Default::default() },
            );
        }
        assert_eq!(drop_count.load(Ordering::SeqCst), 0);
    }

    rcu_run_callbacks();

    // The list is dropped here, so the contained objects should also be dropped.
    assert_eq!(drop_count.load(Ordering::SeqCst), 3);
}

#[test]
fn test_rcu_list_iter() {
    {
        let list = RcuList::<TestNode, TestNode>::default();
        let scope = RcuReadScope::new();
        unsafe {
            list.push_back(&scope, TestNode::new(1));
            list.push_back(&scope, TestNode::new(2));
            list.push_back(&scope, TestNode::new(3));
        }

        let mut iter = list.iter(&scope);
        assert_eq!(iter.next().map(|node| node.value), Some(1));
        assert_eq!(iter.next().map(|node| node.value), Some(2));
        assert_eq!(iter.next().map(|node| node.value), Some(3));
        assert_eq!(iter.next().map(|node| node.value), None);
    }

    rcu_run_callbacks();
}

#[test]
fn test_rcu_list_remove() {
    {
        let list = RcuList::<TestNode, TestNode>::default();
        let scope = RcuReadScope::new();
        unsafe {
            list.push_back(&scope, TestNode::new(1));
            list.push_back(&scope, TestNode::new(2));
            list.push_back(&scope, TestNode::new(3));
        }

        let mut cursor = list.cursor(&scope);
        cursor.advance(); // current is 2
        assert_eq!(cursor.current().map(|node| node.value), Some(2));
        unsafe { cursor.remove() };

        let mut iter = list.iter(&scope);
        assert_eq!(iter.next().map(|node| node.value), Some(1));
        assert_eq!(iter.next().map(|node| node.value), Some(3));
        assert_eq!(iter.next().map(|node| node.value), None);

        // Test removing head
        let mut cursor = list.cursor(&scope);
        unsafe { cursor.remove() };

        let mut iter = list.iter(&scope);
        assert_eq!(iter.next().map(|node| node.value), Some(3));
        assert_eq!(iter.next().map(|node| node.value), None);

        // Test removing tail
        let mut cursor = list.cursor(&scope);
        unsafe { cursor.remove() };

        let mut iter = list.iter(&scope);
        assert_eq!(iter.next().map(|node| node.value), None);
    }

    rcu_run_callbacks();
}

#[test]
fn test_rcu_list_remove_all() {
    {
        let list = RcuList::<TestNode, TestNode>::default();
        let scope = RcuReadScope::new();
        unsafe {
            list.push_back(&scope, TestNode::new(1));
            list.push_back(&scope, TestNode::new(2));
            list.push_back(&scope, TestNode::new(3));
        }

        let mut cursor = list.cursor(&scope);
        while cursor.current().is_some() {
            unsafe { cursor.remove() };
        }

        assert_eq!(list.iter(&scope).next().map(|node| node.value), None);
    }

    rcu_run_callbacks();
}

#[test]
fn test_rcu_list_append() {
    {
        let list1 = RcuList::<TestNode, TestNode>::default();
        let scope = RcuReadScope::new();
        unsafe {
            list1.push_back(&scope, TestNode::new(1));
            list1.push_back(&scope, TestNode::new(2));
        }

        let list2 = RcuList::<TestNode, TestNode>::default();
        unsafe {
            list2.push_back(&scope, TestNode::new(3));
            list2.push_back(&scope, TestNode::new(4));
        }

        unsafe { list1.append(&scope, list2) };

        let mut iter = list1.iter(&scope);
        assert_eq!(iter.next().map(|node| node.value), Some(1));
        assert_eq!(iter.next().map(|node| node.value), Some(2));
        assert_eq!(iter.next().map(|node| node.value), Some(3));
        assert_eq!(iter.next().map(|node| node.value), Some(4));
        assert_eq!(iter.next().map(|node| node.value), None);
    }

    rcu_run_callbacks();
}

#[test]
fn test_rcu_list_append_empty() {
    // Append to an empty list.
    {
        let list1 = RcuList::<TestNode, TestNode>::default();
        let list2 = RcuList::<TestNode, TestNode>::default();
        let scope = RcuReadScope::new();
        unsafe {
            list2.push_back(&scope, TestNode::new(1));
            list2.push_back(&scope, TestNode::new(2));
        }
        unsafe { list1.append(&scope, list2) };

        let mut iter = list1.iter(&scope);
        assert_eq!(iter.next().map(|node| node.value), Some(1));
        assert_eq!(iter.next().map(|node| node.value), Some(2));
        assert_eq!(iter.next().map(|node| node.value), None);
    }
    rcu_run_callbacks();

    // Append an empty list.
    {
        let list1 = RcuList::<TestNode, TestNode>::default();
        let scope = RcuReadScope::new();
        unsafe {
            list1.push_back(&scope, TestNode::new(1));
            list1.push_back(&scope, TestNode::new(2));
        }
        let list2 = RcuList::<TestNode, TestNode>::default();
        unsafe { list1.append(&scope, list2) };

        let mut iter = list1.iter(&scope);
        assert_eq!(iter.next().map(|node| node.value), Some(1));
        assert_eq!(iter.next().map(|node| node.value), Some(2));
        assert_eq!(iter.next().map(|node| node.value), None);
    }
    rcu_run_callbacks();
}

#[test]
fn test_rcu_list_is_empty() {
    {
        let list = RcuList::<TestNode, TestNode>::default();
        let scope = RcuReadScope::new();
        assert!(list.is_empty());

        unsafe {
            list.push_back(&scope, TestNode::new(1));
        }
        assert!(!list.is_empty());

        unsafe {
            list.clear();
        }
        assert!(list.is_empty());
    }

    rcu_run_callbacks();
}

#[test]
fn test_rcu_list_split_off() {
    // Split at the beginning.
    {
        let list = RcuList::<TestNode, TestNode>::default();
        let scope = RcuReadScope::new();
        unsafe {
            list.push_back(&scope, TestNode::new(1));
            list.push_back(&scope, TestNode::new(2));
            list.push_back(&scope, TestNode::new(3));
        }

        let new_list = unsafe { list.split_off(&scope, 0) };

        assert!(list.is_empty());
        let mut new_iter = new_list.iter(&scope);
        assert_eq!(new_iter.next().map(|node| node.value), Some(1));
        assert_eq!(new_iter.next().map(|node| node.value), Some(2));
        assert_eq!(new_iter.next().map(|node| node.value), Some(3));
        assert_eq!(new_iter.next().map(|node| node.value), None);
    }
    rcu_run_callbacks();

    // Split in the middle.
    {
        let list = RcuList::<TestNode, TestNode>::default();
        let scope = RcuReadScope::new();
        unsafe {
            list.push_back(&scope, TestNode::new(1));
            list.push_back(&scope, TestNode::new(2));
            list.push_back(&scope, TestNode::new(3));
            list.push_back(&scope, TestNode::new(4));
        }

        let new_list = unsafe { list.split_off(&scope, 2) };

        let mut iter = list.iter(&scope);
        assert_eq!(iter.next().map(|node| node.value), Some(1));
        assert_eq!(iter.next().map(|node| node.value), Some(2));
        assert_eq!(iter.next().map(|node| node.value), None);

        let mut new_iter = new_list.iter(&scope);
        assert_eq!(new_iter.next().map(|node| node.value), Some(3));
        assert_eq!(new_iter.next().map(|node| node.value), Some(4));
        assert_eq!(new_iter.next().map(|node| node.value), None);
    }
    rcu_run_callbacks();

    // Split at the last element.
    {
        let list = RcuList::<TestNode, TestNode>::default();
        let scope = RcuReadScope::new();
        unsafe {
            list.push_back(&scope, TestNode::new(1));
            list.push_back(&scope, TestNode::new(2));
            list.push_back(&scope, TestNode::new(3));
        }

        let new_list = unsafe { list.split_off(&scope, 2) };

        let mut iter = list.iter(&scope);
        assert_eq!(iter.next().map(|node| node.value), Some(1));
        assert_eq!(iter.next().map(|node| node.value), Some(2));
        assert_eq!(iter.next().map(|node| node.value), None);

        let mut new_iter = new_list.iter(&scope);
        assert_eq!(new_iter.next().map(|node| node.value), Some(3));
        assert_eq!(new_iter.next().map(|node| node.value), None);
    }
    rcu_run_callbacks();

    // Split one past the last element.
    {
        let list = RcuList::<TestNode, TestNode>::default();
        let scope = RcuReadScope::new();
        unsafe {
            list.push_back(&scope, TestNode::new(1));
            list.push_back(&scope, TestNode::new(2));
            list.push_back(&scope, TestNode::new(3));
        }

        let new_list = unsafe { list.split_off(&scope, 3) };

        let mut iter = list.iter(&scope);
        assert_eq!(iter.next().map(|node| node.value), Some(1));
        assert_eq!(iter.next().map(|node| node.value), Some(2));
        assert_eq!(iter.next().map(|node| node.value), Some(3));
        assert_eq!(iter.next().map(|node| node.value), None);

        assert!(new_list.is_empty());
    }
    rcu_run_callbacks();

    // Split far past the end of the list.
    {
        let list = RcuList::<TestNode, TestNode>::default();
        let scope = RcuReadScope::new();
        unsafe {
            list.push_back(&scope, TestNode::new(1));
            list.push_back(&scope, TestNode::new(2));
            list.push_back(&scope, TestNode::new(3));
        }

        let new_list = unsafe { list.split_off(&scope, 10) };

        let mut iter = list.iter(&scope);
        assert_eq!(iter.next().map(|node| node.value), Some(1));
        assert_eq!(iter.next().map(|node| node.value), Some(2));
        assert_eq!(iter.next().map(|node| node.value), Some(3));
        assert_eq!(iter.next().map(|node| node.value), None);

        assert!(new_list.is_empty());
    }
    rcu_run_callbacks();
}
