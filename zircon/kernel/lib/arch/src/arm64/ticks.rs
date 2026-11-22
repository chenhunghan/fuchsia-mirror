// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use regio::arm64::{SysReg, spec};

/// [arm/sysreg]/cntpct_el0: CNTPCT_EL0, Counter-timer Physical Count Register
pub const CNTPCT_EL0: SysReg<spec::CNTPCT_EL0, u64> = SysReg::new();

/// [arm/sysreg]/cntvct_el0: CNTVCT_EL0, Counter-timer Virtual Count Register
pub const CNTVCT_EL0: SysReg<spec::CNTVCT_EL0, u64> = SysReg::new();

/// The earliest possible time sample at boot entry, mirroring the C++
/// `arch::EarlyTicks` in `<lib/arch/arm64/ticks.h>`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EarlyTicks {
    pub cntpct_el0: u64,
    pub cntvct_el0: u64,
}

impl EarlyTicks {
    pub const fn zero() -> Self {
        Self { cntpct_el0: 0, cntvct_el0: 0 }
    }

    #[cfg(target_arch = "aarch64")]
    pub fn get() -> Self {
        Self { cntpct_el0: CNTPCT_EL0.read(), cntvct_el0: CNTVCT_EL0.read() }
    }
}
