// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

extern crate proc_macro;

use proc_macro::TokenStream;

#[proc_macro]
pub fn noop(input: TokenStream) -> TokenStream {
    input
}
