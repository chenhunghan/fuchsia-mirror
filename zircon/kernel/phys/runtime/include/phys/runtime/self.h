// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_RUNTIME_INCLUDE_PHYS_RUNTIME_SELF_H_
#define ZIRCON_KERNEL_PHYS_RUNTIME_INCLUDE_PHYS_RUNTIME_SELF_H_

#include <stdint.h>
#include <zircon/compiler.h>

#include <ktl/byte.h>

class ElfImage;

// These are defined by the linker script and give the bounds of the memory
// image, i.e. the load image plus the reserve_memory_size (bss).
extern "C" __LOCAL ktl::byte PHYS_LOAD_ADDRESS[], _end[];

// Sets up Self() to describe the running phys executable, from the layout
// provided by the linker script.  `name` is the program name used in
// diagnostics and symbolizer markup.  This must be called before Self() or
// PruneFromSelf().
void InitSelf(const char* name);

// The running phys executable's own image.
const ElfImage& Self();

// Removes [start, end) from the self image's description of itself: those
// addresses are no longer considered part of the program.  This is only
// bookkeeping; the memory itself is untouched.  The range must be a trailing
// subrange of the self image's final (data + bss) segment, within its bss.
// Symbolizer context for the self image is re-emitted after updating it.
void PruneFromSelf(uint64_t start, uint64_t end);

#endif  // ZIRCON_KERNEL_PHYS_RUNTIME_INCLUDE_PHYS_RUNTIME_SELF_H_
