// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel UndefinedBehaviorSanitizer (`ubsan`) custom diagnostic reporting runtime.

mod runtime_ffi;

use core::ffi::{VaList, c_char, c_void};
use core::mem::offset_of;

use boot_options::{BootOptions, CheckFailAction};
use kprint::kprint;
use libc::stdio::{fprintf, stdout, vfprintf};

use crate::crashlog::STDOUT_PANIC_BUFFER;
use crate::platform_rs::power::{
    PlatformHaltAction, ZirconCrashReason, platform_halt, platform_panic_start,
};

/// Source location descriptor passed by compiler-generated UBSan checks.
/// Matches `ubsan::SourceLocation` in `<lib/ubsan-custom/types.h>`.
#[repr(C)]
pub(super) struct SourceLocation {
    /// Source file path string, or null for synthesized/unknown locations.
    pub filename: *const c_char,
    /// 1-based line number.
    pub line: u32,
    /// 1-based column number.
    pub column: u32,
}

zr::static_assert_size_and_align!(SourceLocation, 16, 8);
zr::static_assert!(offset_of!(SourceLocation, filename) == 0);
zr::static_assert!(offset_of!(SourceLocation, line) == 8);
zr::static_assert!(offset_of!(SourceLocation, column) == 12);

fn must_panic() -> bool {
    BootOptions::as_ptr().is_null() // Anything so early it's not set yet panics.
        || BootOptions::get().ubsan_action == CheckFailAction::Panic
}

/// Starts a UBSan failure report (`ubsan::Report::Report`).
///
/// # Safety
///
/// `check` must point to a valid NUL-terminated C string. `loc.filename` must be either null or a
/// pointer to a valid NUL-terminated C string.
pub(super) unsafe fn report_start(
    check: *const c_char,
    loc: &SourceLocation,
    caller: *mut c_void,
    frame: *mut c_void,
) {
    let must_panic = must_panic();
    if must_panic {
        platform_panic_start();
    }

    let stream = if must_panic { STDOUT_PANIC_BUFFER.as_mut_ptr() } else { stdout() };
    let kind = if must_panic { c"PANIC".as_ptr() } else { c"OOPS".as_ptr() };
    // SAFETY: `stream` is a valid `File`, the format string is NUL-terminated, and the arguments
    // match `%s`, `%p`, `%p`.
    unsafe {
        fprintf(
            stream,
            c"\n*** ZIRCON KERNEL %s (caller pc: %p, stack frame: %p):\n*** ".as_ptr(),
            kind,
            caller,
            frame,
        );
    }

    let filename = if loc.filename.is_null() { c"<unknown>".as_ptr() } else { loc.filename };

    // SAFETY: `STDOUT_PANIC_BUFFER` is a valid `File`, the format string is NUL-terminated,
    // `filename` is a valid NUL-terminated C string (or the fallback `c"<unknown>"`), and caller
    // guarantees `check` is a valid NUL-terminated C string.
    unsafe {
        fprintf(
            STDOUT_PANIC_BUFFER.as_mut_ptr(),
            c"UBSAN CHECK FAILED at (%s:%d): %s\n".as_ptr(),
            filename,
            loc.line,
            check,
        );
    }
}

/// Finishes a UBSan failure report (`ubsan::Report::~Report`), halting the system on panic.
pub(super) fn report_end() {
    if must_panic() {
        // SAFETY: `STDOUT_PANIC_BUFFER` is a valid `File` and the format string is NUL-terminated.
        unsafe {
            fprintf(STDOUT_PANIC_BUFFER.as_mut_ptr(), c"\n".as_ptr());
        }
        platform_halt(PlatformHaltAction::Halt, ZirconCrashReason::Panic);
    } else {
        kprint!("\n");
    }
}

/// Formats details into the active UBSan failure report (`ubsan::VPrintf`).
///
/// # Safety
///
/// `fmt` must be a valid NUL-terminated printf format string matching `args`.
pub(super) unsafe fn vprintf(fmt: *const c_char, args: VaList<'_>) {
    let stream = if must_panic() { STDOUT_PANIC_BUFFER.as_mut_ptr() } else { stdout() };
    // SAFETY: `stream` is a valid `File` and caller guarantees `fmt` and `args` are valid for
    // `vfprintf`.
    unsafe {
        vfprintf(stream, fmt, args);
    }
}
