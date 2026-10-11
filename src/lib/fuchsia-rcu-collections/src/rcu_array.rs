// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_rcu::{RcuBox, RcuDroppable, RcuReadScope};

/// An array-like data structure that can be read without locking.
///
/// `RcuArray` provides a way to share a growable array between multiple threads.
/// Writers create a new copy of the array when it needs to be grown, and readers
/// are guaranteed to see a consistent snapshot of the array without blocking
/// writers.
#[derive(Default, Debug, RcuDroppable)]
pub struct RcuArray<T: RcuDroppable + Sync> {
    inner: RcuBox<Box<[T]>>,
}

impl<T: RcuDroppable + Sync> RcuArray<T> {
    /// Returns a reference to the element at the given `index`, or `None` if the
    /// index is out of bounds.
    pub fn get<'a>(&self, scope: &'a RcuReadScope, index: usize) -> Option<&'a T> {
        let array = self.inner.as_ref(scope);
        array.get(index)
    }

    /// Returns a slice containing the entire array.
    pub fn as_slice<'a>(&self, scope: &'a RcuReadScope) -> &'a [T] {
        let array = self.inner.as_ref(scope);
        array.as_ref()
    }

    /// Ensures that the array has at least `requested_size` elements, filling with
    /// `value` if the array needs to be grown.
    ///
    /// If the array is already large enough, this function does nothing. Otherwise,
    /// the array is grown to at least `requested_size`. To avoid frequent reallocations,
    /// the array will at least double in size.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn ensure_at_least(&self, requested_size: usize)
    where
        T: Clone + Default,
    {
        let array = self.inner.read();
        if array.len() >= requested_size {
            return;
        }
        let new_size = std::cmp::max(requested_size, array.len() * 2);
        self.copy_update(&array, new_size);
    }

    /// Updates the array to contain the given vector.
    pub fn update(&self, new_array: Vec<T>) {
        self.inner.update(new_array.into_boxed_slice());
    }

    fn copy_update(&self, array: &[T], new_size: usize)
    where
        T: Clone + Default,
    {
        let mut new_array = Vec::new();
        new_array.reserve_exact(new_size);
        for item in array.iter() {
            new_array.push(item.clone());
        }
        for _ in array.len()..new_size {
            new_array.push(T::default());
        }
        self.inner.update(new_array.into_boxed_slice());
    }
}

impl<T: Clone + RcuDroppable + Sync> Clone for RcuArray<T> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

/// Creates an `RcuArray` from a `Vec<T>`.
impl<T: RcuDroppable + Sync> From<Vec<T>> for RcuArray<T> {
    fn from(value: Vec<T>) -> Self {
        Self { inner: RcuBox::new(value.into_boxed_slice()) }
    }
}
