// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel instrumentation runtime support.

#[path = "asan/asan.rs"]
pub mod asan;
#[cfg(ktest)]
#[path = "test/asan_tests.rs"]
mod asan_tests;
pub mod debugdata;
mod instrumentation_ffi;
pub mod kernel_mapped_vmo;
mod memory_profile;
mod profile;
mod profile_ffi;
mod sancov;
mod sancov_ffi;
#[path = "ubsan/runtime.rs"]
mod ubsan;
mod vmo;
mod vmo_ffi;

pub use vmo::InstrumentationData;

use crate::object::HandleOwner;

/// Metadata and handle for an instrumentation VMO published to userboot.
struct InstrumentationDataVmo {
    /// A descriptive string used in the kernel log message.
    announce: &'static str,

    /// Name of the kind of data, matching the fuchsia.debugdata.Publish argument.
    sink_name: &'static str,

    /// The contents are described as "up to N units", where N is bytes/scale.
    units: &'static str,
    /// Scale factor dividing the VMO stream size in bytes to produce the reported unit count.
    scale: usize,

    /// Read-only VMO handle to publish to userboot, if any data is present.
    handle: Option<HandleOwner>,
}

impl Default for InstrumentationDataVmo {
    fn default() -> Self {
        Self { announce: "", sink_name: "", units: "bytes", scale: 1, handle: None }
    }
}
