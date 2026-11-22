// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! x86 hypervisor support.

pub mod guest;
mod guest_ffi;
pub mod invalidate;
mod invalidate_ffi;
pub mod pv;
pub mod vcpu;
pub mod vmcs;
pub mod vmexit;
pub mod vmx_cpu_state;
mod vmx_cpu_state_ffi;
pub mod vmx_state;
