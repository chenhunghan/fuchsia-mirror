// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// The earliest possible time sample at boot entry, mirroring the C++
/// `arch::EarlyTicks` in `<lib/arch/x86/ticks.h>`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EarlyTicks {
    pub tsc: u64,
}

impl EarlyTicks {
    pub const fn zero() -> Self {
        Self { tsc: 0 }
    }

    #[cfg(target_arch = "x86_64")]
    pub fn get() -> Self {
        Self { tsc: super::cycles() }
    }
}
