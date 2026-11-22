// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod manager;
mod state;
mod suspend_stats;
mod sync_on_suspend;
mod wake_lock;
mod wakeup_count;

pub use manager::*;
pub use state::*;
pub use suspend_stats::*;
pub use sync_on_suspend::*;
pub use wake_lock::*;
pub use wakeup_count::*;

/// Returns the total time the system has spent suspended since boot.
///
/// The monotonic clock pauses while the system is suspended, whereas the boot clock keeps
/// advancing, so the difference between the two is the accumulated suspended time. Reading the
/// monotonic clock before the boot clock ensures that any execution delay or suspend between the
/// two reads inflates `boot_ns - mono_ns` and never reduces it.
pub fn suspended_time() -> zx::BootDuration {
    let mono_ns = zx::MonotonicInstant::get().into_nanos();
    let boot_ns = zx::BootInstant::get().into_nanos();
    zx::BootDuration::from_nanos(boot_ns.saturating_sub(mono_ns))
}
