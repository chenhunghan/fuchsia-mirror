// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Printing of 'dump' style information at different depths.

use core::fmt::{self, Write};

/// A [`core::fmt::Write`] implementation that writes to the kernel console.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConsoleSink;

impl Write for ConsoleSink {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        kprint::kprint!("{:s}", s);
        Ok(())
    }
}

/// Formats a single item and emits it to a [`DepthPrinter`].
///
/// This wraps [`DepthPrinter::emit`], building the [`core::fmt::Arguments`] from the given format
/// string and arguments.
///
/// ```ignore
/// dump::emit!(printer, "physical_page_provider {:p} phys_base_ 0x{:x}", self_ptr, phys_base);
/// ```
#[macro_export]
macro_rules! emit {
    ($printer:expr, $($args:tt)*) => {
        $printer.emit(core::format_args!($($args)*))
    };
}

/// Helper for printing 'dump' style information at different depths (i.e. with space prefixes).
///
/// This type is not thread safe.
pub struct DepthPrinter<W: Write> {
    writer: W,
    depth: usize,
    list_max: usize,
    list_emitted: usize,
    in_list: bool,
}

impl<W: Write> DepthPrinter<W> {
    /// Creates a printer emitting items at `depth` to `writer`.
    pub fn new(writer: W, depth: usize) -> Self {
        Self { writer, depth, list_max: 0, list_emitted: 0, in_list: false }
    }

    /// Emit a single item. It will have spaces prefixed based on the depth and a newline added
    /// to the end.
    ///
    /// Use the [`emit!`] macro to construct the [`core::fmt::Arguments`] and emit an item in one
    /// step.
    pub fn emit(&mut self, args: fmt::Arguments<'_>) {
        if self.in_list {
            self.list_emitted += 1;
            if self.list_emitted > self.list_max {
                return;
            }
        }
        self.print_depth();
        let _ = self.writer.write_fmt(args);
        let _ = self.writer.write_str("\n");
    }

    /// Indicates a list is about to be emitted. Only at most `max` emit calls will result in
    /// output, with any additional being counted. [`DepthPrinter::end_list`] must be called once
    /// finished emitting the list.
    pub fn begin_list(&mut self, max: usize) {
        debug_assert!(!self.in_list);
        self.list_max = max;
        self.list_emitted = 0;
        self.in_list = true;
    }

    /// Indicates that printing of the list is finished and, if relevant, will emit a message
    /// indicating how many items were skipped.
    pub fn end_list(&mut self) {
        debug_assert!(self.in_list);
        self.in_list = false;
        if self.list_emitted > self.list_max {
            let not_emitted = self.list_emitted - self.list_max;
            crate::emit!(self, "[{} items not emitted]", not_emitted);
        }
    }

    fn print_depth(&mut self) {
        for _ in 0..self.depth {
            let _ = self.writer.write_str("  ");
        }
    }
}

impl DepthPrinter<ConsoleSink> {
    /// Creates a printer emitting items at `depth` to the kernel console.
    pub fn console(depth: usize) -> Self {
        Self::new(ConsoleSink, depth)
    }
}

impl<W: Write> Drop for DepthPrinter<W> {
    fn drop(&mut self) {
        debug_assert!(!self.in_list);
    }
}

/// Test suite for the Rust `DepthPrinter` implementation.
#[cfg(ktest)]
#[unittest::suite(name = "depth_printer_rust")]
mod tests {
    use crate::DepthPrinter;
    use core::cell::RefCell;
    use core::fmt;
    use fbl::StringBuffer;
    use unittest::{expect_eq, expect_true};

    /// A [`core::fmt::Write`] sink accumulating everything written to it in a [`StringBuffer`],
    /// which can be shared by multiple [`DepthPrinter`]s.
    struct StringSink {
        buffer: RefCell<StringBuffer<2048>>,
    }

    impl StringSink {
        fn new() -> Self {
            Self { buffer: RefCell::new(StringBuffer::new()) }
        }
    }

    impl fmt::Write for &StringSink {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            self.buffer.borrow_mut().write_str(s)
        }
    }

    /// Tests that items are emitted at their depth and that lists elide their excess items.
    #[test]
    fn depth_printer_smoke_test() {
        const EXPECTED: &[u8] = concat!(
            "Hello\n",
            "        World\n",
            "One\n",
            "Two\n",
            "[2 items not emitted]\n",
            "        One\n",
            "        Two\n",
            "        Goodbye\n",
            "foo bar baz\n",
        )
        .as_bytes();

        let buffer = StringSink::new();

        let mut no_depth = DepthPrinter::new(&buffer, 0);
        let mut depth = DepthPrinter::new(&buffer, 4);

        emit!(no_depth, "Hello");
        emit!(depth, "World");
        no_depth.begin_list(2);
        emit!(no_depth, "One");
        emit!(no_depth, "Two");
        emit!(no_depth, "Three");
        emit!(no_depth, "Four");
        no_depth.end_list();
        depth.begin_list(4);
        emit!(depth, "One");
        emit!(depth, "Two");
        depth.end_list();
        emit!(depth, "Goodbye");
        emit!(no_depth, "foo {} baz", "bar");

        let output = buffer.buffer.borrow();
        expect_eq!(EXPECTED.len(), output.len());
        expect_true!(EXPECTED == &output[..]);
    }
}
