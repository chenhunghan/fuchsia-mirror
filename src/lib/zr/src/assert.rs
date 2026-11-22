// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Runtime assertion macros that outline formatted panic paths.
//!
//! Standard [`core::assert!`] and [`core::debug_assert!`] construct [`core::fmt::Arguments`]
//! inline in the caller's stack frame whenever format arguments are present. Even though
//! `core::panicking::panic_fmt` itself is `#[cold]` and `#[inline(never)]`, materializing
//! `fmt::Arguments` and taking references to formatted locals in the caller reserves stack slots
//! (including SafeStack unsafe-stack frames on x86_64) and extra callee-saved registers across the
//! hot path of the enclosing function.
//!
//! [`assert!`] and [`debug_assert!`] move formatted [`core::panic!`] invocations into a `#[cold]`
//! `#[inline(never)]` helper closure that captures values by move/copy, keeping caller stack
//! frames minimal while preserving `const`-context support for unformatted assertions.

/// Out-of-line cold entry point for formatted assertion failures.
///
/// The closure `f` has return type `()` (via a trailing `;` after `core::panic!`) rather than `!`
/// so `rustc` does not mark the closure itself `cold`, allowing LLVM to inline `f` directly into
/// this single `#[cold]` `#[inline(never)]` instantiation without an extra trampoline frame.
#[doc(hidden)]
#[cold]
#[inline(never)]
pub fn __assert_failed<F: FnOnce()>(f: F) -> ! {
    f();
    core::unreachable!()
}

#[doc(hidden)]
#[macro_export]
macro_rules! __assert_outlined {
    ($cond:expr, $($arg:tt)+) => {
        if {
            let __cond: ::core::primitive::bool = $cond;
            !__cond
        } {
            $crate::__assert_failed(move || {
                ::core::panic!($($arg)+);
            });
        }
    };
}

/// Asserts that a boolean expression is `true` at runtime, outlining formatted panic construction
/// into a `#[cold]` `#[inline(never)]` helper.
///
/// Unformatted assertions (`assert!(cond)` and `assert!(cond, "plain literal")`) delegate to
/// [`core::assert!`] so they remain usable in `const` contexts (`const fn` and `const { ... }`).
/// Formatted assertions (`assert!(cond, "x = {x}")` and `assert!(cond, "x = {}", x)`) outline the
/// [`core::fmt::Arguments`] construction so caller stack frames do not reserve stack slots or
/// callee-saved registers for formatting on the non-panicking path.
#[macro_export]
macro_rules! assert {
    ($cond:expr $(,)?) => {
        ::core::assert!($cond)
    };
    ($cond:expr, $msg:literal $(,)?) => {
        $crate::__assert_literal!(($crate), ($cond), $msg)
    };
    ($cond:expr, $($arg:tt)+) => {
        $crate::__assert_outlined!($cond, $($arg)+)
    };
}

/// Asserts that a boolean expression is `true` at runtime when `cfg(debug_assertions)` is enabled,
/// outlining formatted panic construction into a `#[cold]` `#[inline(never)]` helper.
///
/// Like [`core::debug_assert!`], the condition and format arguments are always type-checked even
/// when `debug_assertions` are disabled.
#[macro_export]
macro_rules! debug_assert {
    ($($arg:tt)*) => {
        if ::core::cfg!(debug_assertions) {
            $crate::assert!($($arg)*);
        }
    };
}

#[cfg(test)]
mod tests {
    use core::cell::{Cell, RefCell};

    #[derive(Debug)]
    struct NonCopy(u32);

    const fn const_assertions(val: usize) -> usize {
        assert!(val > 0);
        assert!(val > 1, "plain const message");
        assert!(val > 2, "const message with {{escaped}} braces and \u{1F980}");
        debug_assert!(val > 3);
        debug_assert!(val > 4, "plain const debug message");
        val
    }

    const CONST_CHECK: usize = const_assertions(5);

    #[test]
    fn passing_assertions() {
        assert_eq!(CONST_CHECK, 5);

        let status: Result<(), i32> = Ok(());
        assert!(status.is_ok());
        assert!(status.is_ok(),);
        assert!(status.is_ok(), "plain literal");
        assert!(status.is_ok(), "plain literal trailing comma",);
        assert!(status.is_ok(), "positional {:?}", status);
        assert!(status.is_ok(), "inline {status:?}");
        assert!(status.is_ok(), "named {val:?}", val = status);

        debug_assert!(status.is_ok());
        debug_assert!(status.is_ok(), "debug {:?}", status);
        debug_assert!(status.is_ok(), "debug inline {status:?}");
    }

    #[test]
    fn format_args_not_evaluated_when_passing() {
        let eval_count = Cell::new(0);
        let side_effect = || {
            eval_count.set(eval_count.get() + 1);
            42
        };
        assert!(true, "should not evaluate {}", side_effect());
        debug_assert!(true, "should not evaluate {}", side_effect());
        assert_eq!(eval_count.get(), 0);
    }

    #[test]
    fn non_copy_and_mutable_borrows_remain_usable_after_assert() {
        let item = NonCopy(7);
        assert!(item.0 == 7, "unexpected item {:?}", item);
        debug_assert!(item.0 == 7, "unexpected item {item:?}");
        assert_eq!(item.0, 7);

        let mut value = NonCopy(10);
        let borrow = &mut value;
        assert!(borrow.0 == 10, "unexpected borrow {:?}", borrow);
        borrow.0 = 20;
        assert_eq!(value.0, 20);

        // Temporary borrows created in the condition must drop before the panic closure runs.
        let cell = RefCell::new(3u32);
        assert!(*cell.borrow() == 3, "cell was {}", *cell.borrow());
    }

    #[test]
    #[should_panic(expected = "assertion failed: false")]
    fn assert_no_message_panics() {
        assert!(false);
    }

    #[test]
    #[should_panic(expected = "plain failure with {braces} and 🦀")]
    fn assert_plain_literal_panics() {
        assert!(false, "plain failure with {{braces}} and \u{1F980}");
    }

    #[test]
    #[should_panic(expected = "KernelStack::teardown returned Err(-22)")]
    fn assert_positional_format_panics() {
        let result: Result<(), i32> = Err(-22);
        assert!(result.is_ok(), "KernelStack::teardown returned {:?}", result);
    }

    #[test]
    #[should_panic(expected = "Bad Address 0x0000000000001234")]
    fn assert_inline_capture_format_panics() {
        let addr: u64 = 0x1234;
        assert!(addr == 0, "Bad Address {addr:#018x}");
    }

    #[test]
    #[should_panic(expected = r"raw 0x2a and \0x2a")]
    fn assert_raw_and_escaped_backslash_format_panics() {
        let val: u32 = 0x2a;
        assert!(val == 0, "raw {val:#x} and \\{val:#x}");
    }
}
