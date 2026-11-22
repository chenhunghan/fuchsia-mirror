// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Hypervisor subsystem.

pub mod aspace;
mod aspace_ffi;
pub mod cpu;
mod cpu_ffi;
#[cfg(ktest)]
mod hypervisor_unittest;
pub mod interrupt_tracker;
mod interrupt_tracker_ffi;
pub mod ktrace;
mod ktrace_ffi;
pub mod page;
mod page_ffi;
pub mod state_invalidator;
pub mod trap_map;
mod trap_map_ffi;
