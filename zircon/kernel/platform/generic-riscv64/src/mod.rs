// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! The generic RISC-V 64 platform: the `<platform.h>` entry points, the
//! platform timer, and dispatch of the physboot driver handoff.

pub mod dev_init;
pub mod platform;
pub mod timer;
