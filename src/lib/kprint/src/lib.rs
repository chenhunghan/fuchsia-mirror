// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

#[cfg(test)]
extern crate std;

#[allow(unused_extern_crates)]
extern crate self as kprint;

pub mod backend;

#[doc(hidden)]
pub use kprint_macro::{__kformat_internal, __kprint_internal, __kprintln_internal};

/// Prints formatted text directly to standard kernel console output (via C `printf`).
#[macro_export]
macro_rules! kprint {
    ($($tt:tt)*) => {
        $crate::__kprint_internal!($crate, $($tt)*)
    };
}

/// Prints formatted text followed by a newline to standard kernel console output (via C `printf`).
#[macro_export]
macro_rules! kprintln {
    ($($tt:tt)*) => {
        $crate::__kprintln_internal!($crate, $($tt)*)
    };
}

/// Formats text into a provided buffer (via C `snprintf`) and returns a byte slice (`&[u8]`).
#[macro_export]
macro_rules! kformat {
    ($($tt:tt)*) => {
        $crate::__kformat_internal!($crate, $($tt)*)
    };
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    /// Asserts that `kformat!` produces both `$expected` and the exact same
    /// output as standard Rust `std::format!`.
    macro_rules! assert_kformat_eq {
        ($buf:expr, $expected:expr, $($arg:tt)*) => {{
            let actual = kformat!($buf, $($arg)*);
            let std_out = std::format!($($arg)*);
            assert_eq!(actual, $expected);
            assert_eq!(
                core::str::from_utf8(actual).unwrap(),
                std_out.as_str(),
            );
        }};
    }

    #[test]
    fn test_signed_integers() {
        let mut buf = [0u8; 256];
        assert_kformat_eq!(&mut buf, b"val: -12345", "val: {}", -12345);
        assert_kformat_eq!(&mut buf, b"val: +42", "val: {:+}", 42);
        assert_eq!(kformat!(&mut buf, "val: {:+d}", 42), std::format!("val: {:+}", 42).as_bytes());
        assert_eq!(kformat!(&mut buf, "val: {: d}", 42), b"val:  42");
        assert_kformat_eq!(&mut buf, b"val: '42      '", "val: '{:<8}'", 42);
        assert_eq!(
            kformat!(&mut buf, "val: '{:<8d}'", 42),
            std::format!("val: '{:<8}'", 42).as_bytes()
        );
        assert_kformat_eq!(&mut buf, b"val: '      42'", "val: '{:>8}'", 42);
        assert_eq!(
            kformat!(&mut buf, "val: '{:>8d}'", 42),
            std::format!("val: '{:>8}'", 42).as_bytes()
        );
        assert_kformat_eq!(&mut buf, b"val: '      42'", "val: '{:8}'", 42);
        assert_kformat_eq!(&mut buf, b"val: '      42'", "val: '{:-8}'", 42);
        assert_kformat_eq!(&mut buf, b"val: 00042", "val: {:05}", 42);
        assert_eq!(
            kformat!(&mut buf, "val: {:05d}", 42),
            std::format!("val: {:05}", 42).as_bytes()
        );
        assert_kformat_eq!(&mut buf, b"val: -0000042", "val: {:08}", -42);
        assert_kformat_eq!(&mut buf, b"val: +0000042", "val: {:+08}", 42);
        assert_kformat_eq!(&mut buf, b"val: 00000042", "val: {:<08}", 42);
        assert_kformat_eq!(&mut buf, b"val: 00000042", "val: {:0>8}", 42);
        assert_kformat_eq!(&mut buf, b"val: 42", "val: {:.5}", 42);

        let val_i8: i8 = -8;
        let val_i16: i16 = -16;
        let val_i32: i32 = -32;
        let val_i64: i64 = -64;
        let val_isize: isize = -100;
        assert_kformat_eq!(
            &mut buf,
            b"-8 -16 -32 -64 -100",
            "{} {} {} {} {}",
            &val_i8,
            &val_i16,
            val_i32,
            val_i64,
            val_isize
        );
        assert_kformat_eq!(&mut buf, b"min i64: -9223372036854775808", "min i64: {}", i64::MIN);
    }

    #[test]
    fn test_unsigned_and_hex() {
        let mut buf = [0u8; 256];
        assert_kformat_eq!(&mut buf, b"val: 12345", "val: {}", 12345u32);
        assert_eq!(
            kformat!(&mut buf, "val: {:u}", 12345u32),
            std::format!("val: {}", 12345u32).as_bytes()
        );
        assert_kformat_eq!(&mut buf, b"val: abcd", "val: {:x}", 0xabcd);
        assert_kformat_eq!(&mut buf, b"val: ABCD", "val: {:X}", 0xabcd);
        assert_kformat_eq!(&mut buf, b"val: 0x0", "val: {:#x}", 0);
        assert_kformat_eq!(&mut buf, b"val: 0x2a", "val: {:#x}", 0x2a);
        assert_kformat_eq!(&mut buf, b"val: 0x2A", "val: {:#X}", 0x2a);
        assert_kformat_eq!(&mut buf, b"val: 0x00001A", "val: {:#08X}", 0x1A);
        assert_kformat_eq!(&mut buf, b"val: 0x000000", "val: {:#08x}", 0);
        assert_kformat_eq!(&mut buf, b"val:                0x1", "val: {:#18x}", 1);
        assert_kformat_eq!(&mut buf, b"val:                0x0", "val: {:#18x}", 0);
        assert_kformat_eq!(&mut buf, b"val:     0x2a", "val: {:>#8x}", 0x2a);
        assert_kformat_eq!(&mut buf, b"val:     0x1A", "val: {:#8X}", 0x1a);
        assert_kformat_eq!(&mut buf, b"val: 0x12345", "val: {:#6x}", 0x12345);
        assert_kformat_eq!(&mut buf, b"val: 0xffffffffffffffff", "val: {:#18x}", u64::MAX);
        assert_kformat_eq!(&mut buf, b"val: '0x2a      '", "val: '{:<#10x}'", 0x2a);
        assert_kformat_eq!(&mut buf, b"val: '0x2A      '", "val: '{:<#10X}'", 0x2a);
        assert_kformat_eq!(&mut buf, b"val: '0x0       '", "val: '{:<#10x}'", 0);
        assert_kformat_eq!(&mut buf, b"val: 2a", "val: {:.5x}", 0x2a);
        assert_kformat_eq!(&mut buf, b"val: 100", "val: {:o}", 64);

        // Signed integers formatted as hex use same-width unsigned representation
        assert_kformat_eq!(&mut buf, b"f4", "{:x}", -12i8);
        assert_kformat_eq!(&mut buf, b"0xf4", "{:#x}", -12i8);
        assert_kformat_eq!(&mut buf, b"fb2e", "{:x}", -1234i16);
        assert_kformat_eq!(&mut buf, b"ff439eb2", "{:x}", -12345678i32);
        assert_kformat_eq!(&mut buf, b"ffffffffffffffff", "{:x}", -1i64);

        let val_u8: u8 = 8;
        let val_u16: u16 = 16;
        let val_u32: u32 = 32;
        let val_u64: u64 = 64;
        let val_usize: usize = 128;
        assert_eq!(
            kformat!(
                &mut buf,
                "{:u} {:u} {:u} {:u} {:u}",
                &val_u8,
                val_u16,
                val_u32,
                val_u64,
                val_usize
            ),
            std::format!("{} {} {} {} {}", &val_u8, val_u16, val_u32, val_u64, val_usize)
                .as_bytes()
        );
        assert_eq!(
            kformat!(&mut buf, "max u64: {:u}", u64::MAX),
            std::format!("max u64: {}", u64::MAX).as_bytes()
        );
    }

    #[test]
    fn test_strings_and_cstrings() {
        let mut buf = [0u8; 256];
        assert_kformat_eq!(&mut buf, b"hello world", "hello {}", "world");
        let s = "fuchsia";
        assert_eq!(kformat!(&mut buf, "os: {:s}", s), std::format!("os: {}", s).as_bytes());
        let sub = "pigweed kernel";
        assert_eq!(
            kformat!(&mut buf, "sub: {:s}", &sub[0..7]),
            std::format!("sub: {}", &sub[0..7]).as_bytes()
        );
        assert_kformat_eq!(&mut buf, b"prec: 12345", "prec: {:.5}", "123456789");
        assert_eq!(
            kformat!(&mut buf, "prec: {:.5s}", "123456789"),
            std::format!("prec: {:.5}", "123456789").as_bytes()
        );

        // String alignment, width, and precision
        assert_kformat_eq!(&mut buf, b"'abc     '", "'{:8}'", "abc");
        assert_kformat_eq!(&mut buf, b"'abc     '", "'{:<8}'", "abc");
        assert_kformat_eq!(&mut buf, b"'     abc'", "'{:>8}'", "abc");
        assert_kformat_eq!(&mut buf, b"'abc     '", "'{:08}'", "abc");
        assert_kformat_eq!(&mut buf, b"'abc     '", "'{:8.3}'", "abcdef");
        assert_kformat_eq!(&mut buf, b"'     abc'", "'{:>8.3}'", "abcdef");

        let var_s = "abc";
        assert_eq!(kformat!(&mut buf, "'{:8s}'", var_s), std::format!("'{:8}'", var_s).as_bytes());
        assert_eq!(
            kformat!(&mut buf, "'{:<8s}'", var_s),
            std::format!("'{:<8}'", var_s).as_bytes()
        );
        assert_eq!(
            kformat!(&mut buf, "'{:>8s}'", var_s),
            std::format!("'{:>8}'", var_s).as_bytes()
        );
        assert_eq!(
            kformat!(&mut buf, "'{:08s}'", var_s),
            std::format!("'{:08}'", var_s).as_bytes()
        );

        let byte_slice: &[u8] = b"bytes";
        assert_eq!(kformat!(&mut buf, "raw: {:s}", byte_slice), b"raw: bytes");

        let fixed_array: [u8; 4] = [b'a', b'b', b'c', b'd'];
        assert_eq!(kformat!(&mut buf, "arr: {:s}", fixed_array), b"arr: abcd");

        let c_str = c"zircon-cstr";
        assert_eq!(kformat!(&mut buf, "cstr: {:s}", c_str), b"cstr: zircon-cstr");

        let raw_c_ptr = c"raw-c-ptr".as_ptr();
        assert_eq!(kformat!(&mut buf, "raw_ptr: {:cs}", raw_c_ptr), b"raw_ptr: raw-c-ptr");
        assert_eq!(kformat!(&mut buf, "aligned: {:<12cs}!", raw_c_ptr), b"aligned: raw-c-ptr   !");
        assert_eq!(kformat!(&mut buf, "aligned: {:12cs}!", raw_c_ptr), b"aligned: raw-c-ptr   !");
        assert_eq!(kformat!(&mut buf, "aligned: {:>12cs}!", raw_c_ptr), b"aligned:    raw-c-ptr!");
    }

    #[test]
    fn test_pointers() {
        let mut buf = [0u8; 256];
        let val: i32 = 42;
        let ptr = &val as *const i32;
        assert_eq!(kformat!(&mut buf, "ptr: {:p}", ptr), std::format!("ptr: {:p}", ptr).as_bytes());
        assert_eq!(
            kformat!(&mut buf, "ptr: {:#p}", ptr),
            std::format!("ptr: {:#p}", ptr).as_bytes()
        );

        let usize_addr: usize = 0x12345678;
        let addr_ptr = usize_addr as *const core::ffi::c_void;
        assert_kformat_eq!(&mut buf, b"addr: 0x12345678", "addr: {:p}", addr_ptr);
        assert_eq!(kformat!(&mut buf, "addr: {:p}", usize_addr), b"addr: 0x12345678");
        assert_kformat_eq!(&mut buf, b"addr: 0x0000000012345678", "addr: {:#p}", addr_ptr);
        assert_kformat_eq!(&mut buf, b"addr:     0x12345678", "addr: {:14p}", addr_ptr);
        assert_kformat_eq!(&mut buf, b"addr: 0x12345678    ", "addr: {:<14p}", addr_ptr);
        assert_kformat_eq!(&mut buf, b"addr: 0x000012345678", "addr: {:014p}", addr_ptr);

        let null_ptr: *const core::ffi::c_void = core::ptr::null();
        assert_kformat_eq!(&mut buf, b"null: 0x0", "null: {:p}", null_ptr);
        assert_kformat_eq!(&mut buf, b"null: 0x0000000000000000", "null: {:#p}", null_ptr);
    }

    #[test]
    fn test_single_evaluation() {
        let mut buf = [0u8; 256];
        let mut eval_count = 0;
        let mut side_effect_fn = || {
            eval_count += 1;
            "single_eval"
        };
        let res = kformat!(&mut buf, "result: {:s}", side_effect_fn());
        assert_eq!(res, std::format!("result: {}", "single_eval").as_bytes());
        assert_eq!(eval_count, 1);

        let mut bool_eval_count = 0;
        let mut bool_side_effect_fn = || {
            bool_eval_count += 1;
            true
        };
        let res_b = kformat!(&mut buf, "flag: {:b}", bool_side_effect_fn());
        assert_eq!(res_b, std::format!("flag: {}", true).as_bytes());
        assert_eq!(bool_eval_count, 1);

        let mut multi_eval_count = 0;
        let mut multi_side_effect_fn = || {
            multi_eval_count += 1;
            100
        };
        let res_multi = kformat!(&mut buf, "{0} + {0} = 200", multi_side_effect_fn());
        assert_eq!(res_multi, std::format!("{0} + {0} = 200", 100).as_bytes());
        assert_eq!(multi_eval_count, 1);

        let mut hex_eval_count = 0;
        let mut hex_side_effect_fn = || {
            hex_eval_count += 1;
            0x2au32
        };
        let res_hex = kformat!(&mut buf, "{:#10x}", hex_side_effect_fn());
        assert_eq!(res_hex, std::format!("{:#10x}", 0x2au32).as_bytes());
        assert_eq!(hex_eval_count, 1);
    }

    #[test]
    fn test_concat_format_string() {
        let mut buf = [0u8; 256];
        let val = 42;
        assert_kformat_eq!(
            &mut buf,
            b"prefix_status: 42",
            concat!("prefix_", "status: ", "{}"),
            val
        );
        assert_kformat_eq!(
            &mut buf,
            b"test_1_true_c=99",
            concat!("test_", 1, "_", true, "_", 'c', "={}"),
            99
        );
    }

    #[test]
    fn test_captured_and_named_variables() {
        let mut buf = [0u8; 256];
        let user = "alice";
        let score = 100;
        assert_eq!(
            kformat!(&mut buf, "player {user:s} has score {score}"),
            std::format!("player {user} has score {score}").as_bytes()
        );
        assert_eq!(
            kformat!(&mut buf, "{greeting:s}, {name:s}!", greeting = "hello", name = "world"),
            std::format!("{greeting}, {name}!", greeting = "hello", name = "world").as_bytes()
        );
    }

    #[test]
    fn test_positional_arguments() {
        let mut buf = [0u8; 256];
        assert_kformat_eq!(&mut buf, b"a + b = a", "{0} + {1} = {0}", "a", "b");
        assert_kformat_eq!(&mut buf, b"20 10 20", "{1} {0} {1}", 10, 20);
    }

    #[test]
    fn test_escapes_and_percent() {
        let mut buf = [0u8; 256];
        assert_kformat_eq!(&mut buf, b"{hello}", "{{hello}}");
        assert_kformat_eq!(&mut buf, b"100% completed", "100% completed");
        assert_kformat_eq!(&mut buf, b"75% done", "{}% done", 75);
        assert_kformat_eq!(&mut buf, b"{50%}", "{{{}%}}", 50);
    }

    #[test]
    fn test_chars() {
        let mut buf = [0u8; 256];
        assert_kformat_eq!(&mut buf, b"char: Z", "char: {}", 'Z');
        let c = '?';
        assert_eq!(kformat!(&mut buf, "char: {:c}", c), std::format!("char: {}", c).as_bytes());
        assert_eq!(
            kformat!(&mut buf, "ref char: {:c}", &c),
            std::format!("ref char: {}", &c).as_bytes()
        );

        // Alignment, width, and precision on chars
        assert_kformat_eq!(&mut buf, b"'a     '", "'{:6}'", 'a');
        assert_kformat_eq!(&mut buf, b"'a     '", "'{:<6}'", 'a');
        assert_kformat_eq!(&mut buf, b"'     a'", "'{:>6}'", 'a');
        assert_kformat_eq!(&mut buf, b"'a     '", "'{:06}'", 'a');
        assert_kformat_eq!(&mut buf, b"''", "'{:.0}'", 'a');
        assert_kformat_eq!(&mut buf, b"'a'", "'{:.5}'", 'a');

        assert_eq!(kformat!(&mut buf, "'{:6c}'", c), std::format!("'{:6}'", c).as_bytes());
        assert_eq!(kformat!(&mut buf, "'{:>6c}'", c), std::format!("'{:>6}'", c).as_bytes());
        assert_eq!(kformat!(&mut buf, "'{:.0c}'", c), std::format!("'{:.0}'", c).as_bytes());

        // u8 and byte literals: {} formats byte literal as integer (88), {:c} formats as ASCII character
        let byte: u8 = b'A';
        assert_kformat_eq!(&mut buf, b"byte lit auto: 88", "byte lit auto: {}", b'X');
        assert_eq!(kformat!(&mut buf, "byte: {:c}", byte), b"byte: A");
        assert_eq!(kformat!(&mut buf, "ref byte: {:c}", &byte), b"ref byte: A");
        assert_eq!(kformat!(&mut buf, "byte lit: {:c}", b'K'), b"byte lit: K");

        // Non-ASCII and multi-byte Unicode characters
        assert_kformat_eq!(&mut buf, "crab: 🦀".as_bytes(), "crab: {}", '🦀');
        assert_eq!(kformat!(&mut buf, "accent: {:c}", 'é'), "accent: é".as_bytes());
    }

    #[test]
    fn test_booleans() {
        let mut buf = [0u8; 256];
        assert_kformat_eq!(&mut buf, b"flag: true", "flag: {}", true);
        assert_kformat_eq!(&mut buf, b"flag: false", "flag: {}", false);
        assert_kformat_eq!(&mut buf, b"'true    '", "'{:8}'", true);
        assert_kformat_eq!(&mut buf, b"'    true'", "'{:>8}'", true);
        assert_kformat_eq!(&mut buf, b"'true    '", "'{:08}'", true);
        assert_kformat_eq!(&mut buf, b"'tru'", "'{:.3}'", true);

        let f = false;
        assert_eq!(kformat!(&mut buf, "flag: {:b}", f), std::format!("flag: {}", f).as_bytes());
        assert_eq!(kformat!(&mut buf, "'{:8b}'", f), std::format!("'{:8}'", f).as_bytes());
        assert_eq!(kformat!(&mut buf, "'{:>8b}'", f), std::format!("'{:>8}'", f).as_bytes());
        assert_eq!(kformat!(&mut buf, "'{:.3b}'", f), std::format!("'{:.3}'", f).as_bytes());
    }

    #[test]
    fn test_floats() {
        let mut buf = [0u8; 256];
        assert_eq!(
            kformat!(&mut buf, "pi: {:.2f}", core::f64::consts::PI),
            std::format!("pi: {:.2}", core::f64::consts::PI).as_bytes()
        );
        assert_kformat_eq!(&mut buf, b"float: 1.2346", "float: {:.4}", 1.23456f32);
        assert_eq!(
            kformat!(&mut buf, "float: {:.4f}", 1.23456f32),
            std::format!("float: {:.4}", 1.23456f32).as_bytes()
        );
        assert_kformat_eq!(&mut buf, b"'      1.25'", "'{:10.2}'", 1.25f64);
        assert_kformat_eq!(&mut buf, b"'1.25      '", "'{:<10.2}'", 1.25f64);
        assert_kformat_eq!(&mut buf, b"'-000001.25'", "'{:010.2}'", -1.25f64);
        assert_kformat_eq!(&mut buf, b"'+000001.25'", "'{:+010.2}'", 1.25f64);
        let f = 1000.0;
        let res_e = kformat!(&mut buf, "sci: {:.1e}", f);
        assert_eq!(res_e, b"sci: 1.0e+03");
    }

    #[test]
    fn test_buffer_truncation() {
        let mut small_buf = [0u8; 8];
        let res = kformat!(&mut small_buf, "1234567890");
        assert_eq!(res, b"1234567");
        assert_eq!(res.len(), 7);
    }

    #[test]
    fn test_macro_wrapper_hygiene() {
        macro_rules! wrapped_kformat {
            ($buf:expr, $($tt:tt)*) => {
                kformat!($buf, $($tt)*)
            };
        }
        let mut buf = [0u8; 64];
        let x = 123;
        assert_eq!(wrapped_kformat!(&mut buf, "val: {}", x), b"val: 123");
        assert_eq!(wrapped_kformat!(&mut buf, "val: {}", x), std::format!("val: {}", x).as_bytes());
    }

    #[test]
    fn test_print_macros() {
        kprint!("test printf {}", 123);
        kprint!("100%");
        kprintln!(" line2");
        kprintln!("empty without args");
        kprintln!("{:#x}", 0);
        kprintln!("crab: {}", '🦀');
    }
}
