// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_HANDOFF_END_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_HANDOFF_END_H_

// These functions relate to PhysHandoff but exist only in the kernel proper.

#include <stddef.h>

#include <ktl/type_traits.h>
#include <phys/handoff/handoff.h>

// This is valid to call only with addresses inside the kernel image itself.
// The template wrappers below should always be used instead.  They ensure at
// compile time that only a variable or function in the kernel's image is used.
extern "C" paddr_t KernelPhysicalAddressOf(uintptr_t va);

// The template argument can be any global / static function or variable.
template <auto& Symbol>
inline paddr_t KernelPhysicalAddressOf() {
  return KernelPhysicalAddressOf(reinterpret_cast<uintptr_t>(&Symbol));
}

// This version can be used to index into an array variable.
template <auto& Symbol>
  requires(ktl::is_array_v<ktl::remove_reference_t<decltype(Symbol)>>)
inline paddr_t KernelPhysicalAddressOf(size_t idx) {
  return KernelPhysicalAddressOf(reinterpret_cast<uintptr_t>(&Symbol[idx]));
}

// A non-const variable with all-zero initialized contents (whether default or
// explicitly so) can normally go into bss pages.  These are not guaranteed to
// be physically contiguous with the rest of the kernel image.  So any variable
// that could wind up in bss must be annotated with this macro if it will be
// used with KernelPhysicalAddressOf().
#define KERNEL_PHYSICAL_CONTIGUOUS_DATA [[gnu::section(".data")]]

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_HANDOFF_END_H_
