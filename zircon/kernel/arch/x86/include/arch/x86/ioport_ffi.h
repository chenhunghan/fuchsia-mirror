// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// TODO(https://fxbug.dev/540178114): Remove once all C++ callers are ported to Rust.

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IOPORT_FFI_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IOPORT_FFI_H_

#include <zircon/compiler.h>

#include <kernel/ffi.h>

class IoBitmap;

__BEGIN_CDECLS

void rust_io_bitmap_init(ffi::Uninitialized<IoBitmap>* bitmap);
void rust_io_bitmap_destroy(IoBitmap* bitmap);
void rust_x86_set_tss_io_bitmap(const IoBitmap& bitmap);
void rust_x86_clear_tss_io_bitmap(const IoBitmap& bitmap);

__END_CDECLS

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_IOPORT_FFI_H_
