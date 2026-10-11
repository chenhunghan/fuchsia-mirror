// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2013, Google Inc. All rights reserved.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Debug tracing helpers for the fixed point arithmetic routines, mirroring
//! `<lib/fixed_point_debug.h>`.
//!
//! All of the `debug_*` routines are gated on [`DEBUG_FIXED_POINT`]. When the
//! gate is `false` (the default) the conditionals are constant-folded away and
//! no code or format strings are emitted.

use crate::fixed_point::Fp3264;

/// Compile-time gate for the fixed point debug tracing. Flip to `true` to
/// trace every intermediate multiplication while debugging this crate.
const DEBUG_FIXED_POINT: bool = false;

/// Returns the column padding that precedes a 32-bit operand with the given
/// bit `shift` so that aligned columns line up in the trace output.
fn fpd_shift_prefix_32(shift: i32) -> &'static str {
    match shift {
        32 => "",
        0 => "         ",
        -32 => "                0.",
        -64 => "                0.00000000 ",
        _ => "???",
    }
}

/// Returns the column padding that precedes a 64-bit product with the given
/// bit `shift` so that aligned columns line up in the trace output.
fn fpd_shift_prefix_64(shift: i32) -> &'static str {
    match shift {
        32 => "",
        0 => "         ",
        -32 => "                  ",
        -64 => "                         0.",
        _ => "???",
    }
}

/// Returns the column padding that follows an operand or product with the given
/// bit `shift` so that aligned columns line up in the trace output.
fn fpd_shift_suffix(shift: i32) -> &'static str {
    match shift {
        32 => " 00000000                  ",
        0 => "                  ",
        -32 => "         ",
        -64 => "",
        _ => "???",
    }
}

/// Traces a single `a * b = ret` partial product, annotating each operand with
/// its bit position (`a_shift`, `b_shift`) relative to the binary point.
pub(crate) fn debug_mul_u32_u32(a: u32, b: u32, a_shift: i32, b_shift: i32, ret: u64) {
    if DEBUG_FIXED_POINT {
        debug::tracef!(
            "         {}{:08x}{} * {}{:08x}{} = {}{:08x}{}{:08x}{}\n",
            fpd_shift_prefix_32(a_shift),
            a,
            fpd_shift_suffix(a_shift),
            fpd_shift_prefix_32(b_shift),
            b,
            fpd_shift_suffix(b_shift),
            fpd_shift_prefix_64(a_shift + b_shift),
            (ret >> 32) as u32,
            if a_shift + b_shift == -32 { "." } else { " " },
            ret as u32,
            fpd_shift_suffix(a_shift + b_shift)
        );
    }
}

/// Traces the full and rounded results of [`crate::u64_mul_u32_fp32_64`].
pub(crate) fn debug_u64_mul_u32_fp32_64(a: u32, b: Fp3264, res_0: u64, res_l32_32: u32, ret: u64) {
    if DEBUG_FIXED_POINT {
        debug::tracef!(
            concat!(
                "          {:08x}                   *          {:08x}.{:08x} {:08x}",
                " =          {:08x} {:08x}.{:08x}\n"
            ),
            a,
            b.l0,
            b.l32,
            b.l64,
            (res_0 >> 32) as u32,
            res_0 as u32,
            res_l32_32
        );
        debug::tracef!(
            concat!(
                "                                   ",
                "                                      ",
                "~=          {:08x} {:08x}\n"
            ),
            (ret >> 32) as u32,
            ret as u32
        );
    }
}

/// Traces the full and rounded results of [`crate::u32_mul_u64_fp32_64`].
pub(crate) fn debug_u32_mul_u64_fp32_64(a: u64, b: Fp3264, res_l32: u64, ret: u32) {
    if DEBUG_FIXED_POINT {
        debug::tracef!(
            concat!(
                "{:08x} {:08x}                   *          {:08x}.{:08x} {:08x}",
                " =                   {:08x}.{:08x}\n"
            ),
            (a >> 32) as u32,
            a as u32,
            b.l0,
            b.l32,
            b.l64,
            (res_l32 >> 32) as u32,
            res_l32 as u32
        );
        debug::tracef!(
            concat!(
                "                                   ",
                "                                      ",
                "~=                   {:08x}\n"
            ),
            ret
        );
    }
}

/// Traces the full and rounded results of [`crate::u64_mul_u64_fp32_64`].
pub(crate) fn debug_u64_mul_u64_fp32_64(a: u64, b: Fp3264, res_0: u64, res_l32_32: u32, ret: u64) {
    if DEBUG_FIXED_POINT {
        debug::tracef!(
            concat!(
                "{:08x} {:08x}                   *          {:08x}.{:08x} {:08x}",
                " =          {:08x} {:08x}.{:08x}\n"
            ),
            (a >> 32) as u32,
            a as u32,
            b.l0,
            b.l32,
            b.l64,
            (res_0 >> 32) as u32,
            res_0 as u32,
            res_l32_32
        );
        debug::tracef!(
            concat!(
                "                                   ",
                "                                      ",
                "~=          {:08x} {:08x}\n"
            ),
            (ret >> 32) as u32,
            ret as u32
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_prefix_32() {
        assert_eq!(fpd_shift_prefix_32(32), "");
        assert_eq!(fpd_shift_prefix_32(0), "         ");
        assert_eq!(fpd_shift_prefix_32(-32), "                0.");
        assert_eq!(fpd_shift_prefix_32(-64), "                0.00000000 ");
        assert_eq!(fpd_shift_prefix_32(7), "???");
    }

    #[test]
    fn shift_prefix_64() {
        assert_eq!(fpd_shift_prefix_64(32), "");
        assert_eq!(fpd_shift_prefix_64(0), "         ");
        assert_eq!(fpd_shift_prefix_64(-32), "                  ");
        assert_eq!(fpd_shift_prefix_64(-64), "                         0.");
        assert_eq!(fpd_shift_prefix_64(-1), "???");
    }

    #[test]
    fn shift_suffix() {
        assert_eq!(fpd_shift_suffix(32), " 00000000                  ");
        assert_eq!(fpd_shift_suffix(0), "                  ");
        assert_eq!(fpd_shift_suffix(-32), "         ");
        assert_eq!(fpd_shift_suffix(-64), "");
        assert_eq!(fpd_shift_suffix(64), "???");
    }

    #[test]
    fn debug_helpers_are_callable() {
        // With DEBUG_FIXED_POINT disabled these must be silent no-ops; this just makes sure the (normally dead) code keeps type-checking and running.
        let b = Fp3264 { l0: 1, l32: 0x8000_0000, l64: 0 };
        debug_mul_u32_u32(1, 2, 0, -32, 2);
        debug_u64_mul_u32_fp32_64(1, b, 1, 0x8000_0000, 2);
        debug_u32_mul_u64_fp32_64(1, b, (1u64 << 32) | 0x8000_0000, 2);
        debug_u64_mul_u64_fp32_64(1, b, 1, 0x8000_0000, 2);
    }
}
