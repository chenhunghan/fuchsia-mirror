// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// Observe the referenced data and prevent the compiler from removing previous writes to it.
pub fn optimization_barrier<T: ?Sized>(_val: &T) {}
