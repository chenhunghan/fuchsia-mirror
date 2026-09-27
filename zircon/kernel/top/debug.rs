// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

unsafe extern "C" {
    fn spin(usecs: u32);
}

/// Busy-waits for at least `usecs` microseconds without blocking.
///
/// This does not yield the CPU, so it is usable with interrupts or preemption disabled. Prefer
/// [`crate::kernel::thread::sleep_relative`] wherever blocking is acceptable.
#[inline]
pub fn spin_usecs(usecs: u32) {
    // SAFETY: `spin` is a pure busy-wait on the platform timer with no preconditions.
    unsafe { spin(usecs) }
}
