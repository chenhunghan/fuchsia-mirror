// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Helpers for printing 'dump' style information at different depths.

#![no_std]

mod depth_printer;

pub use depth_printer::{ConsoleSink, DepthPrinter};
