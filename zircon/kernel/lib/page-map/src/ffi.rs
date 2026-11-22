// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::Entry;
use core::ptr::NonNull;

/// Releases a C++ `page_map::Accessor`'s reference to `entry`.  Called by C++
/// `page_map::Accessor::Invalidate`.
///
/// May destroy `*entry`.
///
/// # Safety
///
/// `entry` must point to a live `Entry` on which the caller holds an accessor reference (owned by
/// a C++ `page_map::Accessor` sharing layout with Rust `page_map::Accessor`).  The caller gives up
/// that reference and must not use `entry` afterwards.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_page_map_release_entry(entry: NonNull<Entry>) {
    // SAFETY: The caller upholds `Entry::release`'s contract.
    unsafe { Entry::release(entry) };
}
