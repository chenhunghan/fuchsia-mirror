// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::rcu_array::*;
use fuchsia_rcu::{RcuReadScope, rcu_run_callbacks};

#[test]
fn test_rcu_array_get() {
    let array = RcuArray::from(vec![1, 2, 3]);
    let scope = RcuReadScope::new();
    assert_eq!(array.get(&scope, 0), Some(&1));
    assert_eq!(array.get(&scope, 1), Some(&2));
    assert_eq!(array.get(&scope, 2), Some(&3));
    assert_eq!(array.get(&scope, 3), None);
}

#[test]
fn test_rcu_array_as_slice() {
    let array = RcuArray::from(vec![1, 2, 3]);
    let scope = RcuReadScope::new();
    assert_eq!(array.as_slice(&scope), &[1, 2, 3]);
}

#[test]
fn test_rcu_array_ensure_at_least() {
    let array = RcuArray::from(vec![1, 2, 3]);

    unsafe { array.ensure_at_least(5) };
    let scope = RcuReadScope::new();
    // Should at least double.
    assert_eq!(array.as_slice(&scope), &[1, 2, 3, 0, 0, 0]);

    unsafe { array.ensure_at_least(2) };

    // Should not shrink below current size.
    assert_eq!(array.as_slice(&scope), &[1, 2, 3, 0, 0, 0]);

    unsafe { array.ensure_at_least(12) };
    assert_eq!(array.as_slice(&scope), &[1, 2, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

    std::mem::drop(scope);
    rcu_run_callbacks();
}

#[test]
fn test_rcu_array_from_vec() {
    let vec = vec![1, 2, 3];
    let array = RcuArray::from(vec.clone());
    let scope = RcuReadScope::new();
    assert_eq!(array.as_slice(&scope), vec.as_slice());
}
