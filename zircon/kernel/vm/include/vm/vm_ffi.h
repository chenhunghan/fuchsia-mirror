// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_VM_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_VM_FFI_H_

#include <stdbool.h>
#include <stddef.h>
#include <sys/types.h>
#include <zircon/compiler.h>

// TODO(https://fxbug.dev/565806419): Remove once VM initialization is ported to Rust.
__BEGIN_CDECLS

paddr_t cpp_vaddr_to_paddr(const void* va);
bool cpp_vm_using_virtual_heap();
vaddr_t cpp_vm_get_kernel_heap_base();
size_t cpp_vm_get_kernel_heap_size();

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_VM_FFI_H_
