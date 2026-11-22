// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod apic_id;
mod bug;
mod cache;
pub mod cpuid;
mod extension;
mod feature;
#[cfg(target_arch = "x86_64")]
mod intrin;
mod power;
mod speculation;
mod system;
mod ticks;

pub use apic_id::*;
pub use bug::*;
pub use cache::*;
pub use extension::*;
pub use feature::*;
#[cfg(target_arch = "x86_64")]
pub use intrin::*;
pub use power::*;
pub use speculation::*;
pub use system::*;
pub use ticks::*;

/// Enumeration of vendors.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Vendor {
    Intel,
    Amd,
    Unknown,
}

impl Vendor {
    /// Returns the human-readable vendor name (`arch::ToString(Vendor)`).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::Intel => "Intel",
            Self::Amd => "AMD",
        }
    }
}

impl core::fmt::Display for Vendor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}
