// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![warn(unsafe_op_in_unsafe_fn)]

use fuchsia_rcu::subtle::{RcuPtr, RcuPtrRef};
use fuchsia_rcu::{RcuDroppable, RcuReadScope, rcu_drop};

use crate::rcu_intrusive_list::{Link, RcuIntrusiveList, RcuIntrusiveListCursor, RcuListAdapter};

/// An `RcuList` is a doubly-linked list that supports concurrent access via
/// read-copy-update (RCU) synchronization.
///
/// An `RcuList` can be safely read by multiple readers, even while a writer
/// is modifying the list. To read from the list, you will need to enter an
/// `RcuReadScope`.
///
/// To modify the list, you will need to use some external synchronization,
/// such as a `Mutex`, to exclude concurrent writers.
#[derive(Debug)]
pub struct RcuList<T: RcuDroppable + Sync, A: RcuListAdapter<T>> {
    list: RcuIntrusiveList<T, A>,
}

// SAFETY: RcuList drops all elements through its intrusive list nodes, which are of type T
// (implementing RcuDroppable).
unsafe impl<T: RcuDroppable + Sync, A: RcuListAdapter<T> + Send + Sync + 'static> RcuDroppable
    for RcuList<T, A>
{
}

impl<T: RcuDroppable + Sync, A: RcuListAdapter<T>> Default for RcuList<T, A> {
    fn default() -> Self {
        Self { list: RcuIntrusiveList::default() }
    }
}

impl<T: RcuDroppable + Sync, A: RcuListAdapter<T>> RcuList<T, A> {
    /// Creates a new list with the given head and tail.
    pub fn new(head: RcuPtr<Link>, tail: RcuPtr<Link>) -> Self {
        Self { list: RcuIntrusiveList::new(head, tail) }
    }

    /// Pushes a new element to the front of the list.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn push_front<'a>(&self, scope: &'a RcuReadScope, data: T) -> RcuPtrRef<'a, T> {
        let node = alloc(scope, data);
        // SAFETY: Our caller promises to exclude concurrent writers.
        unsafe {
            self.list.push_front(scope, node);
        }
        node
    }

    /// Pushes a new element to the back of the list.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn push_back<'a>(&self, scope: &'a RcuReadScope, data: T) -> RcuPtrRef<'a, T> {
        let node = alloc(scope, data);
        // SAFETY: Our caller promises to exclude concurrent writers.
        unsafe {
            self.list.push_back(scope, node);
        }
        node
    }

    /// Appends another list to the end of this list.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn append(&self, scope: &RcuReadScope, other: Self) {
        // SAFETY: Our caller promises to exclude concurrent writers.
        unsafe {
            let items = other.list.split_off(scope, 0);
            self.list.append(scope, items);
        }
    }

    /// Splits the list into two lists at the given position.
    ///
    /// If the given position is past the end of the list, returns an empty list.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn split_off(&self, scope: &RcuReadScope, pos: usize) -> Self {
        // SAFETY: Our caller promises to exclude concurrent writers.
        Self { list: unsafe { self.list.split_off(scope, pos) } }
    }

    /// Removes all elements from the list.
    ///
    /// Concurrent readers may continue to see the old value of the list until the RCU state machine
    /// has made sufficient progress to ensure that no concurrent readers are holding read guards.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn clear(&self) {
        let scope = RcuReadScope::new();
        // SAFETY: Our caller promises to exclude concurrent writers.
        unsafe { self.list.clear(&scope, deferred_dealloc) };
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        let scope = RcuReadScope::new();
        self.list.is_empty(&scope)
    }

    /// Returns a cursor that can be used to traverse and modify the list.
    ///
    /// Concurrent readers may continue to see the old value of the list until the RCU state machine
    /// has made sufficient progress to ensure that no concurrent readers are holding read guards.
    pub fn cursor<'a>(&'a self, scope: &'a RcuReadScope) -> RcuListCursor<'a, T, A> {
        RcuListCursor { cursor: self.list.cursor(scope) }
    }

    /// Returns an iterator over the elements in the list.
    pub fn iter<'a>(&self, scope: &'a RcuReadScope) -> impl Iterator<Item = &'a T> {
        self.list.iter(scope)
    }
}

/// Allocates a new node.
///
/// The node must be deallocated using `deferred_dealloc`.
fn alloc<T>(scope: &RcuReadScope, data: T) -> RcuPtrRef<'_, T> {
    let ptr = Box::into_raw(Box::new(data));
    // SAFETY: All nodes must be deallocated using `deferred_dealloc`, which defers their
    // deallocation until all in-flight read operations have completed.
    unsafe { RcuPtrRef::new(scope, ptr) }
}

/// Deallocates a node once all in-flight read operations have completed.
///
/// The node must have been allocated using `alloc`.
fn deferred_dealloc<T>(node: RcuPtrRef<'_, T>)
where
    T: RcuDroppable + Sync,
{
    // SAFETY: The node was allocated using `alloc`.
    let value = unsafe { Box::from_raw(node.as_mut_ptr()) };
    rcu_drop(value);
}

pub struct RcuListCursor<'a, T: RcuDroppable + Sync, A: RcuListAdapter<T>> {
    cursor: RcuIntrusiveListCursor<'a, T, A>,
}

impl<'a, T: RcuDroppable + Sync, A: RcuListAdapter<T>> RcuListCursor<'a, T, A> {
    /// Returns the element at the current cursor position.
    pub fn current(&self) -> Option<&T> {
        self.cursor.current()
    }

    /// Advances the cursor to the next element in the list.
    pub fn advance(&mut self) {
        self.cursor.advance();
    }

    /// Removes the element at the current cursor position.
    ///
    /// After calling `remove`, the cursor will be positioned at the next element in the list.
    ///
    /// Concurrent readers may continue to see this entry in the list until the RCU state machine
    /// has made sufficient progress to ensure that no concurrent readers are holding read guards.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn remove(&mut self) -> RcuPtrRef<'a, T> {
        let removed = unsafe { self.cursor.remove() };
        deferred_dealloc(removed);
        removed
    }
}

impl<T: RcuDroppable + Sync, A: RcuListAdapter<T>> Drop for RcuList<T, A> {
    fn drop(&mut self) {
        // SAFETY: The list is being dropped, so there are no concurrent readers.
        unsafe { self.clear() };
    }
}
