// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// backtrace tests
#[unittest::suite(name = "backtrace_rust")]
mod backtrace_tests {
    use super::super::{Backtrace, FrameType};
    use libc::stdio::StringFile;
    use unittest::{assert_eq, expect_false, expect_true};

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|window| window == needle)
    }

    /// VectorOps
    #[test]
    fn vector_ops() {
        let mut bt = Backtrace::default();
        assert_eq!(0, bt.size());
        expect_true!(bt.data().is_empty());
        bt.reset();
        assert_eq!(0, bt.size());
        expect_true!(bt.data().is_empty());

        for x in 0..Backtrace::MAX_SIZE {
            bt.push_back(x);
            assert_eq!(x, bt.size() - 1);
        }
        let expected: [usize; Backtrace::MAX_SIZE] = core::array::from_fn(|i| i);
        expect_true!(bt.data() == expected);

        // Pushing beyond MAX_SIZE is ignored and leaves the contents unchanged.
        bt.push_back(Backtrace::MAX_SIZE);
        assert_eq!(Backtrace::MAX_SIZE, bt.size());
        expect_true!(bt.data() == expected);

        bt.reset();
        assert_eq!(0, bt.size());
        expect_true!(bt.data().is_empty());
    }

    /// Print
    #[test]
    fn print() {
        let mut bt = Backtrace::default();
        bt.push_back(0xffffffff76543210);
        bt.push_back(0xffffffff76543214);
        bt.push_back(0xffffffff76543218);
        let mut buffer = [0u8; 1024];

        // All frames are return addresses.
        {
            let mut file = StringFile::new(&mut buffer);
            file.with_file(|f| {
                // SAFETY: `f.as_mut_ptr()` is non-null and points to an initialized `StringFile`
                // `FILE` whose non-null write callback writes into `file` without mutating the
                // `FILE` and remains valid for the duration of the call.
                unsafe { bt.print_to_file(f.as_mut_ptr()) };
            });
            expect_true!(contains(file.as_bytes(), b"zx_system_get_version_string"));
            expect_true!(contains(file.as_bytes(), b"{{{bt:0:0xffffffff76543210:ra}}}\n"));
            expect_true!(contains(file.as_bytes(), b"{{{bt:1:0xffffffff76543214:ra}}}\n"));
            expect_true!(contains(file.as_bytes(), b"{{{bt:2:0xffffffff76543218:ra}}}\n"));
        }

        buffer.fill(0);

        // And now with the first frame as a precise location.
        {
            let mut file = StringFile::new(&mut buffer);
            bt.set_first_frame_type(FrameType::PreciseLocation);
            file.with_file(|f| {
                // SAFETY: `f.as_mut_ptr()` is non-null and points to an initialized `StringFile`
                // `FILE` whose non-null write callback writes into `file` without mutating the
                // `FILE` and remains valid for the duration of the call.
                unsafe { bt.print_to_file(f.as_mut_ptr()) };
            });
            expect_true!(contains(file.as_bytes(), b"zx_system_get_version_string"));
            expect_true!(contains(file.as_bytes(), b"{{{bt:0:0xffffffff76543210:pc}}}\n"));
            expect_true!(contains(file.as_bytes(), b"{{{bt:1:0xffffffff76543214:ra}}}\n"));
            expect_true!(contains(file.as_bytes(), b"{{{bt:2:0xffffffff76543218:ra}}}\n"));
        }

        buffer.fill(0);

        // A zero address formats via `%p` as `0`.
        {
            let mut zero_bt = Backtrace::default();
            zero_bt.push_back(0);
            let mut file = StringFile::new(&mut buffer);
            file.with_file(|f| {
                // SAFETY: `f.as_mut_ptr()` is non-null and points to an initialized `StringFile`
                // `FILE` whose non-null write callback writes into `file` without mutating the
                // `FILE` and remains valid for the duration of the call.
                unsafe { zero_bt.print_to_file(f.as_mut_ptr()) };
            });
            expect_true!(contains(file.as_bytes(), b"{{{bt:0:0:ra}}}\n"));
        }
    }

    /// PrintWithoutVersion
    #[test]
    fn print_without_version() {
        let mut bt = Backtrace::default();
        bt.push_back(0xffffffff76543210);

        let mut buffer = [0u8; 1024];
        let mut file = StringFile::new(&mut buffer);
        file.with_file(|f| {
            // SAFETY: `f.as_mut_ptr()` is non-null and points to an initialized `StringFile`
            // `FILE` whose non-null write callback writes into `file` without mutating the `FILE`
            // and remains valid for the duration of the call.
            unsafe { bt.print_without_version(f.as_mut_ptr()) };
        });
        expect_false!(contains(file.as_bytes(), b"zx_system_get_version_string"));
        expect_true!(contains(file.as_bytes(), b"{{{bt:0:0xffffffff76543210:ra}}}\n"));
    }
}
