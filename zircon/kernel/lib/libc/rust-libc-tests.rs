// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#![no_std]
#![allow(clippy::write_literal)]

use core::fmt::Write as _;
use core::write;
use libc::stdio::FILE;

#[unsafe(no_mangle)]
pub extern "C" fn write_to_stdio_in_rust(f: &mut FILE) -> bool {
    write!(f, "{} {}!", "Hello", "world").is_ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn write_to_stdio_with_nul_in_rust(f: &mut FILE) -> bool {
    write!(f, "{}\0{}!", "Hello", "world").is_ok()
}
