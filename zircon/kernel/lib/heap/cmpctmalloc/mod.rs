// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Compact kernel heap allocator (`cmpctmalloc`).

#![cfg_attr(feature = "userspace", no_std)]

#[cfg(test)]
extern crate std;

#[cfg(feature = "userspace")]
#[macro_use(assert, debug_assert)]
extern crate zr;

#[cfg_attr(not(feature = "userspace"), expect(clippy::module_inception))]
mod cmpctmalloc;
#[cfg(test)]
#[path = "tests/cmpctmalloc_test.rs"]
mod tests;

pub use cmpctmalloc::*;
