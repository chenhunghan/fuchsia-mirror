// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Inspect property recording helpers.

use fuchsia_inspect::Node as InspectNode;

/// Records a slice of signed integer properties on `node`.
#[inline(never)]
pub fn record_ints(node: &InspectNode, items: &[(&str, i64)]) {
    for &(name, value) in items {
        node.record_int(name, value);
    }
}
