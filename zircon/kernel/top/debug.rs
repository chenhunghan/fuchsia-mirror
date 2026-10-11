// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::fmt::{self, Display, Formatter};

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

/// One chunk of the filler line in the `k crash rust_panic` message.
const CRASH_PANIC_FILLER_CHUNK: &str =
    "................................................................";

/// Number of filler chunks. Together they make the message several hundred bytes
/// long, while the whole panic output still fits in the 2048-byte crashlog panic
/// buffer.
const CRASH_PANIC_FILLER_CHUNK_COUNT: usize = 8;

/// Renders the filler line piecewise, so the panic message is formatted rather
/// than a literal and has to be streamed through the panic handler.
struct CrashPanicFiller;

impl Display for CrashPanicFiller {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        for _ in 0..CRASH_PANIC_FILLER_CHUNK_COUNT {
            formatter.write_str(CRASH_PANIC_FILLER_CHUNK)?;
        }
        Ok(())
    }
}

/// Deliberately panics through the Rust panic handler. Backs `k crash rust_panic`.
///
/// The message ends with a marker on its own line, which tests use to check that
/// a long formatted message reaches the console and crashlog untruncated.
#[unsafe(no_mangle)]
pub extern "C" fn rust_debug_crash_panic() -> ! {
    panic!("rust_panic test message:\n{CrashPanicFiller}\nend of rust_panic test message");
}
