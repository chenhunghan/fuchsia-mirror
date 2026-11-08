// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#![cfg_attr(not(test), no_std)]

#[cfg(test)]
use kprint as _;

// For use in `kernel_oops!`.
#[doc(hidden)]
pub use kprint::kprint as __kprint;

pub mod dprintf;
pub mod ltrace;

/// Banner which starts every `DRIVER OOPS` message (`tefmocheck` looks for this in logs).
// LINT.IfChange
pub const DRIVER_OOPS_BANNER: &str = "\nDRIVER OOPS\n";
// LINT.ThenChange(
//   //zircon/kernel/include/debug.h,
//   //tools/testing/tefmocheck/string_in_log_check.go,
// )

/// Banner which starts every `DRIVER WARNING` message.
// LINT.IfChange
pub const DRIVER_WARN_BANNER: &str = "\nDRIVER WARNING\n";
// LINT.ThenChange(//zircon/kernel/include/debug.h)

/// Zero-sized [`core::fmt::Write`] adapter that writes directly to the kernel console.
pub struct KernelConsoleWriter;

impl core::fmt::Write for KernelConsoleWriter {
    #[cfg(not(test))]
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        kprint::kprint!("{:s}", s);
        Ok(())
    }

    #[cfg(test)]
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        std::print!("{}", s);
        Ok(())
    }
}

/// Fixed-size stack buffer implementing [`core::fmt::Write`] that truncates on a UTF-8 character
/// boundary when full.
struct StackBufWriter<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> StackBufWriter<N> {
    const fn new() -> Self {
        Self { buf: [0u8; N], len: 0 }
    }

    fn as_str(&self) -> &str {
        // SAFETY: `write_str` only ever appends valid UTF-8 substrings (truncated on a character
        // boundary) into `self.buf[..self.len]`.
        unsafe { core::str::from_utf8_unchecked(&self.buf[..self.len]) }
    }
}

impl<const N: usize> core::fmt::Write for StackBufWriter<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let avail = N - self.len;
        let mut to_copy = s.len().min(avail);
        while !s.is_char_boundary(to_copy) {
            to_copy -= 1;
        }
        self.buf[self.len..self.len + to_copy].copy_from_slice(&s.as_bytes()[..to_copy]);
        self.len += to_copy;
        Ok(())
    }
}

#[doc(hidden)]
#[cold]
#[inline(never)]
pub fn print_with_banner(banner: &str, args: core::fmt::Arguments<'_>) {
    use core::fmt::Write as _;
    let mut buf = StackBufWriter::<512>::new();
    let _ = buf.write_str(banner);
    let _ = buf.write_fmt(args);
    let _ = KernelConsoleWriter.write_str(buf.as_str());
}

/// A [`kprint`]-like macro which prepend's the user's message with the special
/// "ZIRCON KERNEL OOPS" tag. Automated builds look for tags like this and
/// consider their presence to indicate test failures, even if the higher level
/// test framework code thinks the test passed.
///
/// A kernel OOPS indicates the presence of a bug, however, the bug may not be
/// in the kernel itself.
///
/// # Examples
/// ```rust
/// kernel_oops!("value {} is out of range", val);
/// ```
#[macro_export]
macro_rules! kernel_oops {
    ($fmt:literal, $($arg:tt)*) => {
        $crate::__kprint!(concat!("\nZIRCON KERNEL OOPS\n", $fmt), $($arg)*)
    };
}

/// Formats a message prefixed with [`DRIVER_OOPS_BANNER`] into a stack buffer and emits it in a
/// single console write so concurrent CPUs cannot interleave output between the banner and message
/// body.
///
/// Equivalent to C++ `DRIVER_OOPS` in `<debug.h>`.
#[macro_export]
macro_rules! driver_oops {
    ($($arg:tt)*) => {
        $crate::print_with_banner($crate::DRIVER_OOPS_BANNER, core::format_args!($($arg)*))
    };
}

/// Formats a message prefixed with [`DRIVER_WARN_BANNER`] into a stack buffer and emits it in a
/// single console write.
///
/// Equivalent to C++ `DRIVER_WARN` in `<debug.h>`.
#[macro_export]
macro_rules! driver_warn {
    ($($arg:tt)*) => {
        $crate::print_with_banner($crate::DRIVER_WARN_BANNER, core::format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt::Write as _;

    #[test]
    fn test_stack_buf_writer_utf8_truncation() {
        let mut w = StackBufWriter::<6>::new();
        let _ = w.write_str("ab🦀cd");
        // "ab" is 2 bytes; "🦀" is 4 bytes (fits in 6), "cd" is truncated.
        assert_eq!(w.as_str(), "ab🦀");

        let mut w2 = StackBufWriter::<5>::new();
        let _ = w2.write_str("ab🦀cd");
        // "🦀" (4 bytes) does not fit in the remaining 3 bytes, so truncation stops on the
        // character boundary after "ab".
        assert_eq!(w2.as_str(), "ab");
    }

    #[test]
    fn test_driver_oops_and_warn() {
        driver_oops!("test oops {}\n", 42);
        driver_warn!("test warn {}\n", "msg");
    }
}
