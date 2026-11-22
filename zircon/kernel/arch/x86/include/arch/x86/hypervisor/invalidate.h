// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_HYPERVISOR_INVALIDATE_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_HYPERVISOR_INVALIDATE_H_

#include <zircon/types.h>

// Implemented in Rust in arch/x86/hypervisor/invalidate.rs; exported from invalidate_ffi.rs.
extern "C" {
uint64_t rust_ept_pointer_from_pml4(zx_paddr_t ept_pml4);
}  // extern "C"

// Implemented in Rust in arch/x86/hypervisor/vmx_cpu_state.rs; exported from vmx_cpu_state_ffi.rs.
extern "C" {
void rust_broadcast_invept(uint64_t eptp);
}  // extern "C"

// Returns an EPT pointer based on an EPT PML4 address.
inline uint64_t ept_pointer_from_pml4(zx_paddr_t ept_pml4) {
  return rust_ept_pointer_from_pml4(ept_pml4);
}

// Performs CPU invalidations of the EPT TLB state using the given EPT pointer.
// This invalidates on all necessary CPUs and will perform IPIs.
inline void broadcast_invept(uint64_t eptp) { rust_broadcast_invept(eptp); }

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_HYPERVISOR_INVALIDATE_H_
