// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::iter::Iterator;
use std::marker::PhantomData;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

/// Header at the beginning of every heap-allocated stack node.
pub(crate) struct StackNodeHeader {
    /// The next node in the stack.
    next: *mut StackNodeHeader,
}

impl StackNodeHeader {
    pub(crate) const fn new() -> Self {
        Self { next: ptr::null_mut() }
    }
}

/// A type whose instances own a heap-allocated node beginning with [`StackNodeHeader`].
///
/// # Safety
/// Implementors must guarantee that:
/// - `into_raw` returns a valid, non-null pointer to a [`StackNodeHeader`] that is uniquely
///   owned by the caller and remains valid for reads and writes until reclaimed by `from_raw`.
/// - The pointer returned by `into_raw` carries provenance for the entire underlying allocation
///   (not just the header), since `from_raw` may cast it back to the containing node type.
/// - Between `into_raw` and `from_raw`, nothing other than the holder of the pointer accesses
///   the header. [`AtomicStack`] writes the `next` field during this window.
/// - `from_raw` reconstructs the owning instance from a pointer previously returned by
///   `into_raw`, taking back ownership of the allocation.
pub(crate) unsafe trait StackNode: Sized + Send {
    fn into_raw(self) -> *mut StackNodeHeader;

    /// # Safety
    /// `ptr` must have been returned by a prior call to `Self::into_raw` and must not have been
    /// passed to `from_raw` already.
    unsafe fn from_raw(ptr: *mut StackNodeHeader) -> Self;
}

/// A stack of items that is thread-safe.
///
/// This stack is a singly linked list that is thread-safe and lock-free.
pub(crate) struct AtomicStack<T: StackNode> {
    /// The top element of the stack.
    head: AtomicPtr<StackNodeHeader>,
    _marker: PhantomData<T>,
}

impl<T: StackNode> Default for AtomicStack<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: StackNode> AtomicStack<T> {
    /// Create an empty stack.
    pub(crate) const fn new() -> Self {
        Self { head: AtomicPtr::new(ptr::null_mut()), _marker: PhantomData }
    }

    /// Returns true if the stack is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.head.load(Ordering::Acquire).is_null()
    }

    /// Push an element onto the front of the stack.
    pub(crate) fn push_front(&self, data: T) {
        let node_ptr = data.into_raw();
        // SAFETY: Per the `StackNode` contract, `node_ptr` is valid for reads and writes and is
        // uniquely owned by us until it is published to `self.head` by the successful
        // `compare_exchange` below. `node` is not used after that point.
        let node = unsafe { &mut *node_ptr };
        loop {
            let head = self.head.load(Ordering::Relaxed);
            node.next = head;
            // This uses Release ordering to synchronize with the Acquire in `take_head`.  We need
            // all writes to the element prior to here to be visible in another thread that takes
            // the stack.
            if self
                .head
                .compare_exchange(head, node_ptr, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }

    /// Swap the head of the stack with a null pointer.
    ///
    /// This function empties the stack. The caller takes ownership of the returned nodes.
    fn take_head(&self) -> *mut StackNodeHeader {
        self.head.swap(std::ptr::null_mut(), Ordering::Acquire)
    }

    /// Takes the contents of the stack and returns them as an iterator.
    ///
    /// This function empties the stack.
    pub fn take(&self) -> AtomicListIterator<T> {
        let head = self.take_head();
        AtomicListIterator { head, _marker: PhantomData }
    }

    /// Takes the contents of the stack and returns them as a vector.
    ///
    /// This function empties the stack.
    #[cfg(test)]
    pub(crate) fn drain(&self) -> Vec<T> {
        self.take().collect()
    }
}

impl<T: StackNode> Drop for AtomicStack<T> {
    fn drop(&mut self) {
        for item in self.take() {
            std::mem::drop(item);
        }
    }
}

pub struct AtomicListIterator<T: StackNode> {
    // This pointer is the owning reference to the node.
    head: *mut StackNodeHeader,
    _marker: PhantomData<T>,
}

impl<T: StackNode> AtomicListIterator<T> {
    /// Returns an empty iterator.
    pub const fn empty() -> Self {
        Self { head: std::ptr::null_mut(), _marker: PhantomData }
    }

    /// Returns true if the iterator is empty.
    pub fn is_empty(&self) -> bool {
        self.head.is_null()
    }
}

// SAFETY: The iterator exclusively owns its nodes, each of which logically holds a `T`. Moving the
// iterator to another thread moves those `T`s, which is sound because `StackNode` requires `Send`.
unsafe impl<T: StackNode> Send for AtomicListIterator<T> {}

impl<T: StackNode> Iterator for AtomicListIterator<T> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> {
        if self.head.is_null() {
            None
        } else {
            let current = self.head;
            // SAFETY: `current` is non-null and was produced by `T::into_raw` in `push_front`.
            // The iterator exclusively owns it, so reading the header is sound. We read `next`
            // before handing ownership to `T::from_raw`.
            self.head = unsafe { (*current).next };
            // SAFETY: `current` was returned by `T::into_raw` and has not been reclaimed yet.
            // Ownership is transferred to the returned `T`, and the iterator no longer refers
            // to it.
            Some(unsafe { T::from_raw(current) })
        }
    }
}

impl<T: StackNode> Drop for AtomicListIterator<T> {
    fn drop(&mut self) {
        for item in self {
            std::mem::drop(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    #[repr(C)]
    struct TestValueNode<T> {
        header: StackNodeHeader,
        data: T,
    }

    /// Implements `StackNode` for a test type by boxing it in a `TestValueNode`.
    macro_rules! impl_test_stack_node {
        ($t:ty) => {
            // SAFETY: `TestValueNode<$t>` is `#[repr(C)]` with `header: StackNodeHeader` at
            // offset 0, so a pointer to the node is also a valid pointer to its header. The
            // pointer comes straight from `Box::into_raw` and is only cast, so it retains
            // provenance over the whole allocation.
            unsafe impl StackNode for $t {
                fn into_raw(self) -> *mut StackNodeHeader {
                    Box::into_raw(Box::new(TestValueNode {
                        header: StackNodeHeader::new(),
                        data: self,
                    }))
                    .cast::<StackNodeHeader>()
                }

                unsafe fn from_raw(ptr: *mut StackNodeHeader) -> Self {
                    // SAFETY: `ptr` was returned by `into_raw` above, which allocated a
                    // `Box<TestValueNode<Self>>`, and ownership is transferred back here.
                    let node = unsafe { Box::from_raw(ptr.cast::<TestValueNode<Self>>()) };
                    node.data
                }
            }
        };
    }

    impl_test_stack_node!(i32);
    impl_test_stack_node!(LeakCounter);

    #[test]
    fn test_atomic_list() {
        let list = AtomicStack::new();
        list.push_front(1);
        list.push_front(2);
        list.push_front(3);
        assert_eq!(list.drain(), vec![3, 2, 1]);
        assert_eq!(list.drain(), vec![]);
        list.push_front(4);
        assert_eq!(list.drain(), vec![4]);
        assert_eq!(list.drain(), vec![]);
    }

    #[derive(Debug)]
    struct LeakCounter {
        drop_counter: Arc<AtomicUsize>,
    }

    impl Drop for LeakCounter {
        fn drop(&mut self) {
            self.drop_counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl PartialEq for LeakCounter {
        fn eq(&self, other: &Self) -> bool {
            Arc::ptr_eq(&self.drop_counter, &other.drop_counter)
        }
    }

    impl Eq for LeakCounter {}

    #[test]
    fn test_drain_drops_items() {
        let drop_counter = Arc::new(AtomicUsize::new(0));
        let list = AtomicStack::new();
        list.push_front(LeakCounter { drop_counter: drop_counter.clone() });
        list.push_front(LeakCounter { drop_counter: drop_counter.clone() });
        assert_eq!(drop_counter.load(Ordering::Relaxed), 0);
        list.drain();
        assert_eq!(drop_counter.load(Ordering::Relaxed), 2);
        assert_eq!(list.drain(), vec![]);
        assert_eq!(drop_counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_drop_drops_items() {
        let drop_counter = Arc::new(AtomicUsize::new(0));
        let list = AtomicStack::new();
        list.push_front(LeakCounter { drop_counter: drop_counter.clone() });
        list.push_front(LeakCounter { drop_counter: drop_counter.clone() });
        assert_eq!(drop_counter.load(Ordering::Relaxed), 0);
        drop(list);
        assert_eq!(drop_counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_iterator_drops_items() {
        let drop_counter = Arc::new(AtomicUsize::new(0));
        let list = AtomicStack::new();
        list.push_front(LeakCounter { drop_counter: drop_counter.clone() });
        list.push_front(LeakCounter { drop_counter: drop_counter.clone() });
        assert_eq!(drop_counter.load(Ordering::Relaxed), 0);
        let iter = list.take();
        assert_eq!(drop_counter.load(Ordering::Relaxed), 0);
        drop(iter);
        assert_eq!(drop_counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_is_empty() {
        let list = AtomicStack::new();
        assert!(list.is_empty());
        list.push_front(1);
        assert!(!list.is_empty());
        let mut iter = list.take();
        assert!(list.is_empty());
        assert!(!iter.is_empty());
        assert_eq!(iter.next(), Some(1));
        assert!(iter.is_empty());
        assert_eq!(iter.next(), None);

        let empty_iter = AtomicListIterator::<i32>::empty();
        assert!(empty_iter.is_empty());
    }
}
