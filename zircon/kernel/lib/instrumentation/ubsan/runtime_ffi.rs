// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! C FFI entrypoints for `ubsan::Report` and `ubsan::VPrintf`.

use core::ffi::{VaList, c_char, c_void};

use super::{SourceLocation, report_end, report_start, vprintf};

/// Starts a UBSan failure report (`ubsan::Report::Report`).
///
/// # Safety
///
/// `check` must point to a valid NUL-terminated C string. `loc.filename` must be either null or a
/// pointer to a valid NUL-terminated C string.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_ubsan_report_start(
    check: *const c_char,
    loc: &SourceLocation,
    caller: *mut c_void,
    frame: *mut c_void,
) {
    // SAFETY: Caller upholds `report_start` safety requirements.
    unsafe {
        report_start(check, loc, caller, frame);
    }
}

/// Finishes a UBSan failure report (`ubsan::Report::~Report`).
#[unsafe(no_mangle)]
extern "C" fn rust_ubsan_report_end() {
    report_end();
}

/// Formats details into the active UBSan failure report (`ubsan::VPrintf`).
///
/// # Safety
///
/// `fmt` must be a valid NUL-terminated printf format string matching `args`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_ubsan_vprintf(fmt: *const c_char, args: VaList<'_>) {
    // SAFETY: Caller upholds `vprintf` safety requirements.
    unsafe {
        vprintf(fmt, args);
    }
}
