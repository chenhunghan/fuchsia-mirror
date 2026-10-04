# kprint

`kprint` is a zero-overhead, stack-efficient logging and formatting crate
designed for systems and embedded Rust code in the Zircon kernel environment.

It allows developers to write ergonomic Rust format strings (`{}`) that produce
the **same output as standard Rust formatting (`core::fmt`)** while compiling
invocations directly down to C `printf` / `snprintf` calls without intermediate
stack buffers.

## Overview & Architecture

Standard Rust formatting (`core::fmt::write!`, `format_args!`, and `println!`)
relies heavily on `core::fmt::Formatter`, trait objects, dynamic dispatch
tables, and intermediate stack formatting buffers. In resource-constrained
environments or bare-metal kernels, this introduces unwanted binary bloat and
stack space overhead.

`kprint` solves this by translating Rust format strings and arguments at compile
time into C `printf` / `snprintf` invocations that preserve Rust's formatting
output semantics:
1. **Rust Output Fidelity**: Accuracy is defined by matching the output that
   standard Rust formatting (`core::fmt`) would produce, rather than performing
   a naive 1-to-1 syntax mapping to C `printf` specifiers. Where C `printf`
   differs from Rust (for example, C's `%#x` omits the `0x` prefix when
   formatting `0` and `%#X` emits uppercase `0X`, whereas Rust's `{:#x}` and
   `{:#X}` always emit lowercase `0x` (`0x0`, `0x2A`), and strings default to
   left-alignment), `kprint` synthesizes the appropriate C format string and
   arguments (such as `"0x%llx"`, `"0x%06llX"`, `"%*s%llx"` with a computed
   prefix width, or `"%-8.*s"`) so the resulting output matches Rust without
   allocating stack buffers.
2. **Compile-Time Format Translation**: A procedural macro parses Rust format
   strings (e.g., `"{:04x}"` or `"{:10s}"`) and converts them into static,
   null-terminated C format string literals (`b"%04llx\0"` or `b"%-10.*s\0"`).
3. **Trait-Based Argument Marshaling**: Formatting arguments are bound once to
   local variables and converted via lightweight backend traits and helpers
   (`AsKPrintStr`, `AsKPrintCString`, `AsKPrintSignedInt`,
   `AsKPrintUnsignedInt`, `AsKPrintPointer`, `AsKPrintChar`,
   `hex_alt_prefix_width`).
4. **Minimal Stack & Zero Heap Allocation**: When formatting slices (`&str`,
   `&[u8]`, `&CStr`), `kprint` passes `(slice.len(), slice.as_ptr())` to C
   `%.*s` specifiers, avoiding heap allocations or temporary string-copying
   buffers.
5. **Single Evaluation Semantics**: Every format expression is evaluated
   strictly once, guaranteeing safe usage with side-effecting expressions.
6. **Direct C Backend**: Invocations compile directly into `printf` calls (or
   `snprintf` for `kformat!`), avoiding `core::fmt` dispatch tables.

## Usage

### Basic Printing

```rust
use kprint::{kprint, kprintln};

fn example() {
    let score = 42;
    let user = "alice";

    // Standard output without newline using captured variables
    kprint!("Connecting user {user:s} with ID {score:#06x}...");

    // Standard output with newline
    kprintln!("Connection established.");
}
```

### In-Memory Formatting (`kformat!`)

When you need to format into a stack buffer without allocating:

```rust
use kprint::kformat;

fn format_example() {
    let mut buffer = [0u8; 128];
    let result: &[u8] = kformat!(&mut buffer, "Status: {:#08X} ({})", 0x1A, true);
    assert_eq!(result, b"Status: 0x00001A (true)");
}
```

### Advanced Formatting Features

```rust
// Compile-time string concatenation
kprintln!(concat!("prefix_", "status: {}"), 42);

// Named and positional arguments
kprintln!("{0} + {1} = {0}", "a", "b");
kprintln!("{greeting:s}, {name:s}!", greeting = "hello", name = "world");

// Null-terminated C string pointers (*const c_char)
let c_ptr = c"kernel".as_ptr();
kprintln!("booting {:cs}", c_ptr);
```

## Supported Format Variations

The procedural macro infers C format families from literal types, cast
expressions, or explicit format specifiers, translating them into C `printf`
specifiers and arguments that match Rust's formatting behavior:

| Rust Specifier / Type | Generated C Specifier | C ABI Argument Cast / Trait | Example Output |
| :--- | :--- | :--- | :--- |
| `{}`, `{:d}`, `i32`, `isize` | `%lld` | `AsKPrintSignedInt` (`c_longlong`) | `-12345` |
| `{:u}`, `u32`, `usize` | `%llu` | `AsKPrintUnsignedInt` (`c_ulonglong`) | `12345` |
| `{:x}`, `{:X}` | `%llx`, `%llX` | `AsKPrintUnsignedInt` (`c_ulonglong`) | `abcd`, `ABCD` |
| `{:#x}`, `{:#X}` | `0x%llx`, `0x%llX` | `AsKPrintUnsignedInt` (`c_ulonglong`) | `0x0`, `0x2A` |
| `{:#08X}` | `0x%06llX` | `AsKPrintUnsignedInt` (`c_ulonglong`) | `0x00001A` |
| `{:<#10x}` | `0x%-8llx` | `AsKPrintUnsignedInt` (`c_ulonglong`) | `0x2a      ` |
| `{:#8x}`, `{:>#8x}` | `%*s%llx` | `hex_alt_prefix_width`, `"0x"`, `AsKPrintUnsignedInt` | `    0x1a`, `     0x0` |
| `{:p}`, `{:#p}` | `0x%llx`, `0x%016llx` | `AsKPrintPointer` (`*const c_void` as `c_ulonglong`) | `0x0`, `0x0000000012345678` |
| `{:o}` | `%llo` | `AsKPrintUnsignedInt` (`c_ulonglong`) | `100` |
| `{:s}`, `&str`, `&[u8]`, `&CStr` | `%.*s` | `AsKPrintStr` (`c_int`, `*const c_char`) | `fuchsia` |
| `{:cs}`, `{:z}`, `*const c_char` | `%s` | `AsKPrintCString` (`*const c_char`) | `null-terminated` |
| `{:c}`, `char`, `u8` | `%.*s` | `AsKPrintChar` (`c_int`, `*const c_char`) | `Z`, `A`, `🦀` |
| `{:b}`, `bool` | `%.*s` | length (`4` or `5`), string pointer | `true`, `false` |
| `{:.2f}`, `f64` | `%.2f` | `c_double` | `3.14` |

## Limitations & Intentional Differences from `core::fmt`

To avoid Rust-side formatting loops and intermediate stack buffers, `kprint`
delegates all formatting to C `printf` / `snprintf` and has the following
intentional limitations:

- **Byte-Based Width and Precision**: For strings (`{:s}`) and characters
  (`{:c}`), field width and precision are measured in **UTF-8 bytes** (as
  understood by C `printf`'s `%.*s`) rather than Unicode scalar values (`char`s).
- **Explicit Type Specifiers for Non-Literals**: Because procedural macros run
  prior to Rust type checking, non-literal and non-cast expressions default to
  signed integer formatting (`%lld`) unless an explicit type specifier (`:s`,
  `:u`, `:x`, `:X`, `:p`, `:c`, `:b`, `:f`, `:e`, `:E`, `:cs`) is provided.
- **No Custom Fill Characters or Center Alignment**: Only space and `'0'`
  padding with left (`<`) or right (`>`) alignment are supported by C `printf`.
  Specifying a custom fill character (such as `{:_>8}`) or center alignment
  (`{:^8}`) produces a compile-time error.
- **64-Bit Integer Width**: Integer specifiers convert arguments to
  `c_longlong` / `c_ulonglong` (64-bit).
- **Direct C `printf` Delegation for `:o`, `:f`, `:e`, `:E`, and `:b`**:
  - `{:b}` formats `bool` values (`"true"` / `"false"`) rather than base-2
    integers.
  - `{:o}`, `{:f}`, `{:e}`, and `{:E}` map directly to C `printf`'s `%llo`,
    `%f`, `%e`, and `%E` (e.g., `{:.1e}` formats the exponent using C's
    `1.0e+03` notation).
