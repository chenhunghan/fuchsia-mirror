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
mod pv_ffi;
