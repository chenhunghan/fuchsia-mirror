// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

unsafe extern "C" {
    fn build_flags_rust_c_helper_value() -> i32;
}

fn main() {
    let val = unsafe { build_flags_rust_c_helper_value() };
    assert_eq!(val, 42);
}
