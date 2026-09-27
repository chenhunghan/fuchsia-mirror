// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_inspect::{Error, Node};
use std::collections::VecDeque;

/// This struct is intended to represent a list node in Inspect, which doesn't support list
/// natively. Furthermore, it makes sure that the number of items does not exceed |capacity|
///
/// Each item in `BoundedListNode` is represented as a child node with name as index. This
/// index is always increasing and does not wrap around. For example, if capacity is 3,
/// then the children names are `[0, 1, 2]` on first three addition. When a new node is
/// added, `0` is popped, and the children names are `[1, 2, 3]`.
#[derive(Debug)]
pub struct BoundedListNode {
    node: Node,
    index: usize,
    capacity: usize,
    items: VecDeque<Node>,
}

impl BoundedListNode {
    /// Create a new BoundedListNode with capacity 1 or |capacity|, whichever is larger.
    pub fn new(node: Node, capacity: usize) -> Self {
        Self {
            node,
            index: 0,
            capacity: std::cmp::max(capacity, 1),
            items: VecDeque::with_capacity(capacity),
        }
    }

    /// Returns how many children are in the `BoundedListNode`.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Returns whether or not the bounded list has no elements inside.
    pub fn is_empty(&self) -> bool {
        self.items.len() == 0
    }

    /// Returns the capacity of the `BoundedListNode`, the maximum number of child nodes.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Create a new entry within a list and return a writer that creates properties or children
    /// for this entry. The writer does not have to be kept for the created properties and
    /// children to be maintained in the list.
    ///
    /// If creating new entry exceeds capacity of the list, the oldest entry is evicted.
    ///
    /// The `initialize` function will be used to atomically initialize all children and properties
    /// under the node.
    pub fn add_entry<F>(&mut self, initialize: F) -> &Node
    where
        F: FnOnce(&Node),
    {
        if self.items.len() >= self.capacity {
            self.items.pop_front();
        }

        let entry_node = self.node.atomic_update(|node| {
            let child = node.create_child(self.index.to_string());
            initialize(&child);
            child
        });
        self.items.push_back(entry_node);

        self.index += 1;
        self.items.back().unwrap()
    }

    /// Adopt an existing node as a new entry within a list, renaming it to the next index in the
    /// list, and return a writer that creates properties or children for this entry. The writer
    /// does not have to be kept for the created properties and children to be maintained in the
    /// list.
    ///
    /// If adopting the entry exceeds capacity of the list, the oldest entry is evicted.
    pub fn adopt_entry(&mut self, entry: Node) -> Result<&Node, Error> {
        self.node.atomic_update(|node| {
            node.adopt(&entry)?;
            entry.rename(self.index.to_string())
        })?;

        if self.items.len() >= self.capacity {
            self.items.pop_front();
        }

        self.items.push_back(entry);
        self.index += 1;
        Ok(self.items.back().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use diagnostics_assertions::assert_data_tree;
    use fuchsia_inspect::Inspector;
    use fuchsia_inspect::reader::{self, ReaderError};
    use std::sync::mpsc;

    #[fuchsia::test]
    async fn test_bounded_list_node_basic() {
        let inspector = Inspector::default();
        let list_node = inspector.root().create_child("list_node");
        let mut list_node = BoundedListNode::new(list_node, 3);
        assert_eq!(list_node.capacity(), 3);
        assert_eq!(list_node.len(), 0);
        let _ = list_node.add_entry(|_| {});
        assert_eq!(list_node.len(), 1);
        assert_data_tree!(inspector, root: { list_node: { "0": {} } });
        let _ = list_node.add_entry(|_| {});
        assert_eq!(list_node.len(), 2);
        assert_data_tree!(inspector, root: { list_node: { "0": {}, "1": {} } });
    }

    #[fuchsia::test]
    async fn test_bounded_list_node_eviction() {
        let inspector = Inspector::default();
        let list_node = inspector.root().create_child("list_node");
        let mut list_node = BoundedListNode::new(list_node, 3);
        let _ = list_node.add_entry(|_| {});
        let _ = list_node.add_entry(|_| {});
        let _ = list_node.add_entry(|_| {});

        assert_data_tree!(inspector, root: { list_node: { "0": {}, "1": {}, "2": {} } });
        assert_eq!(list_node.len(), 3);

        let _ = list_node.add_entry(|_| {});
        assert_data_tree!(inspector, root: { list_node: { "1": {}, "2": {}, "3": {} } });
        assert_eq!(list_node.len(), 3);

        let _ = list_node.add_entry(|_| {});
        assert_data_tree!(inspector, root: { list_node: { "2": {}, "3": {}, "4": {} } });
        assert_eq!(list_node.len(), 3);
    }

    #[fuchsia::test]
    async fn test_bounded_list_node_specified_zero_capacity() {
        let inspector = Inspector::default();
        let list_node = inspector.root().create_child("list_node");
        let mut list_node = BoundedListNode::new(list_node, 0);
        let _ = list_node.add_entry(|_| {});
        assert_data_tree!(inspector, root: { list_node: { "0": {} } });
        let _ = list_node.add_entry(|_| {});
        assert_data_tree!(inspector, root: { list_node: { "1": {} } });
    }

    #[fuchsia::test]
    async fn test_bounded_list_node_holds_its_values() {
        let inspector = Inspector::default();
        let list_node = inspector.root().create_child("list_node");
        let mut list_node = BoundedListNode::new(list_node, 3);

        {
            let node_writer = list_node.add_entry(|_| {});
            node_writer.record_string("str_key", "str_value");
            node_writer.record_child("child", |child| child.record_int("int_key", 2));
        } // <-- node_writer is dropped

        // verify list node 0 is still in the tree
        assert_data_tree!(inspector, root: {
            list_node: {
                "0": {
                    str_key: "str_value",
                    child: {
                        int_key: 2i64,
                    }
                }
            }
        });
    }

    #[fuchsia::test]
    async fn add_entry_is_atomic() {
        let inspector = Inspector::default();
        let list_node = inspector.root().create_child("list_node");
        let mut list_node = BoundedListNode::new(list_node, 3);

        let (sender, receiver) = mpsc::channel();
        let (sender2, receiver2) = mpsc::channel();

        let t = std::thread::spawn(move || {
            list_node.add_entry(|node| {
                node.record_string("key1", "value1");
                sender.send(()).unwrap();
                receiver2.recv().unwrap();
                node.record_string("key2", "value2");
            });
            list_node
        });

        // Make sure we already called `add_entry`.
        receiver.recv().unwrap();

        // We can't read until the atomic transaction is completed.
        assert_matches!(reader::read(&inspector).await, Err(ReaderError::InconsistentSnapshot));

        // Let `add_entry` continue executing and wait for completion.
        sender2.send(()).unwrap();

        // Ensure we don't drop the list node.
        let _list_node = t.join().unwrap();

        // We can now read and we can see that everything was correctly created.
        assert_data_tree!(inspector, root: {
            list_node: {
                "0": {
                    key1: "value1",
                    key2: "value2",
                }
            }
        });
    }

    #[fuchsia::test]
    async fn test_bounded_list_node_adopt_entry() {
        let inspector = Inspector::default();
        let list_node = inspector.root().create_child("list_node");
        let mut list_node = BoundedListNode::new(list_node, 3);

        let first = inspector.root().create_child("first");
        first.record_int("val", 10);
        assert_data_tree!(inspector, root: {
            list_node: {},
            first: { val: 10i64 },
        });

        let adopted = list_node.adopt_entry(first).unwrap();
        adopted.record_string("extra", "hello");
        assert_eq!(list_node.len(), 1);
        assert_data_tree!(inspector, root: {
            list_node: {
                "0": {
                    val: 10i64,
                    extra: "hello",
                },
            },
        });

        list_node.add_entry(|n| n.record_int("val", 20));
        let third = inspector.root().create_child("third");
        third.record_int("val", 30);
        list_node.adopt_entry(third).unwrap();
        assert_eq!(list_node.len(), 3);
        assert_data_tree!(inspector, root: {
            list_node: {
                "0": {
                    val: 10i64,
                    extra: "hello",
                },
                "1": {
                    val: 20i64,
                },
                "2": {
                    val: 30i64,
                },
            },
        });

        // Adopting a fourth entry should evict "0" and rename the adopted node to "3".
        let fourth = inspector.root().create_child("fourth");
        fourth.record_int("val", 40);
        list_node.adopt_entry(fourth).unwrap();
        assert_eq!(list_node.len(), 3);
        assert_data_tree!(inspector, root: {
            list_node: {
                "1": {
                    val: 20i64,
                },
                "2": {
                    val: 30i64,
                },
                "3": {
                    val: 40i64,
                },
            },
        });
    }

    #[fuchsia::test]
    async fn test_bounded_list_node_adopt_entry_error() {
        let inspector = Inspector::default();
        let parent = inspector.root().create_child("parent");
        let list_node = parent.create_child("list_node");
        let mut list_node = BoundedListNode::new(list_node, 1);

        list_node.add_entry(|n| n.record_int("val", 1));
        assert_eq!(list_node.len(), 1);

        // Adopting from a different inspector VMO fails and preserves existing list state.
        let other_inspector = Inspector::default();
        let other_node = other_inspector.root().create_child("other");
        assert_matches!(list_node.adopt_entry(other_node), Err(Error::AdoptionIntoWrongVmo));
        assert_data_tree!(inspector, root: {
            parent: {
                list_node: {
                    "0": {
                        val: 1i64,
                    },
                },
            },
        });

        // Adopting an ancestor fails and preserves existing list state.
        assert_matches!(list_node.adopt_entry(parent.clone_weak()), Err(Error::AdoptAncestor));
        assert_data_tree!(inspector, root: {
            parent: {
                list_node: {
                    "0": {
                        val: 1i64,
                    },
                },
            },
        });

        // The next index is still 1.
        let valid_node = inspector.root().create_child("valid");
        valid_node.record_int("val", 2);
        list_node.adopt_entry(valid_node).unwrap();
        assert_data_tree!(inspector, root: {
            parent: {
                list_node: {
                    "1": {
                        val: 2i64,
                    },
                },
            },
        });
    }
}
