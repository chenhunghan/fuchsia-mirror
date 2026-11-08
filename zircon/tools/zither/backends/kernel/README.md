# The Zither Kernel Backend

The Zither kernel backend is responsible for generating syscall-related glue
logic for the kernel and vDSO, derived from the FIDL syscall specifications of
library `zx`.

## Output layout

Most generated files are .inc files with legacy 'macro template' APIs, allowing
for further, flexible, generation logic:
* `<lib/syscalls/category.inc>`: a categorization of syscalls with macro
  helpers;
* `<lib/syscalls/kernel.inc>`: macro-templated enumeration of syscalls
  from the kernel's point of view;
* `<lib/syscalls/syscalls.inc>`: macro-templated enumeration of syscalls
  from userspace's point of view;
* `<lib/syscalls/zx-syscall-numbers.h>`: `#define`s of syscall numbers.

The kernel's syscall dispatch is written in Rust, so the remaining files are
Rust sources for the kernel crate to `include!`, given by their paths under the
output directory:
* `lib/syscalls/kernel_wrappers.rs`: the kernel-side `wrapper_<syscall>`
  entry points called by the architecture syscall dispatcher;
* `lib/syscalls/syscall_numbers.rs`: syscall number constants and the
  interned syscall name table;
* `lib/syscalls/syscall_sigs.rs`: the kernel-side signature of each
  syscall's `sys_<syscall>` implementation.

The C++ outputs are formatted by the backend's `cppFormatter` (clang-format)
and the Rust outputs by its separate `rustFormatter` (rustfmt). (GN and Bazel
backend specifications still spell them `formatter` and `rust_formatter`.)

## GN integration

`${fidl_target}_zither.kernel` generates a C++ library target aggregating the
generated sources, and giving the headers and .inc files as public
dependencies.

## Bindings

Any declaration type not mentioned below is ignored.

### Syscalls

Each syscall declaration yields one 'entry' in each of the generated files.
