// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use regio::riscv64::{Csr, encoding};

/// [riscv/unpriv]: 10.1  Base Counters and Timers (Zicntr)
pub const TIME: Csr<encoding::time, u64> = Csr::new();

/// The earliest possible time sample at boot entry, mirroring the C++
/// `arch::EarlyTicks` in `<lib/arch/riscv64/ticks.h>`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EarlyTicks {
    pub time: u64,
}

impl EarlyTicks {
    pub const fn zero() -> Self {
        Self { time: 0 }
    }

    #[cfg(target_arch = "riscv64")]
    pub fn get() -> Self {
        Self { time: TIME.read() }
    }
}
