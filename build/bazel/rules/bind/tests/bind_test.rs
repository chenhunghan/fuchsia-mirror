// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[test]
fn parent_constants() {
    assert_eq!(bind_test_bindlib_parent::FLAVOR, "test.bindlib.parent.Flavor");
    assert_eq!(bind_test_bindlib_parent::FLAVOR_VANILLA, "vanilla");
    assert_eq!(bind_test_bindlib_parent::SIZE_LARGE, 2);
}

#[test]
fn child_constants() {
    assert_eq!(bind_test_bindlib_child::ENABLED, "test.bindlib.child.enabled");
    assert_eq!(bind_test_bindlib_child::SIZE_MEDIUM, 3);
    assert_eq!(
        bind_test_bindlib_child::bind_test_bindlib_parent::FLAVOR,
        "test.bindlib.parent.Flavor"
    );
}
