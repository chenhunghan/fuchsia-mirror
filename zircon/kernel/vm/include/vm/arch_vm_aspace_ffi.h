// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_ARCH_VM_ASPACE_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_ARCH_VM_ASPACE_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include <arch/aspace.h>
#include <vm/arch_vm_aspace.h>

__BEGIN_CDECLS

zx_status_t cpp_arch_vm_aspace_init(ArchVmAspace* aspace);
zx_status_t cpp_arch_vm_aspace_init_shared(ArchVmAspace* aspace);
zx_status_t cpp_arch_vm_aspace_init_restricted(ArchVmAspace* aspace);
zx_status_t cpp_arch_vm_aspace_init_unified(ArchVmAspace* aspace, ArchVmAspace* shared,
                                            ArchVmAspace* restricted);
void cpp_arch_vm_aspace_disable_updates(ArchVmAspace* aspace);
zx_status_t cpp_arch_vm_aspace_destroy(ArchVmAspace* aspace);
zx_status_t cpp_arch_vm_aspace_map_contiguous(ArchVmAspace* aspace, vaddr_t vaddr, paddr_t paddr,
                                              size_t count, arch_mmu_flags_t mmu_flags);
zx_status_t cpp_arch_vm_aspace_map(ArchVmAspace* aspace, vaddr_t vaddr, paddr_t* phys, size_t count,
                                   arch_mmu_flags_t mmu_flags,
                                   ArchVmAspaceInterface::ExistingEntryAction existing_action);
zx_status_t cpp_arch_vm_aspace_unmap(ArchVmAspace* aspace, vaddr_t vaddr, size_t count,
                                     ArchVmAspaceInterface::ArchUnmapOptions enlarge);
bool cpp_arch_vm_aspace_unmap_only_enlarge_on_oom(ArchVmAspace* aspace);
zx_status_t cpp_arch_vm_aspace_protect(ArchVmAspace* aspace, vaddr_t vaddr, size_t count,
                                       arch_mmu_flags_t mmu_flags,
                                       ArchVmAspaceInterface::ArchUnmapOptions enlarge);
zx_status_t cpp_arch_vm_aspace_query(ArchVmAspace* aspace, vaddr_t vaddr, paddr_t* paddr,
                                     arch_mmu_flags_t* mmu_flags);
vaddr_t cpp_arch_vm_aspace_pick_spot(ArchVmAspace* aspace, vaddr_t base, vaddr_t end, vaddr_t align,
                                     size_t size, arch_mmu_flags_t mmu_flags);
zx_status_t cpp_arch_vm_aspace_harvest_accessed(
    ArchVmAspace* aspace, vaddr_t vaddr, size_t count,
    ArchVmAspaceInterface::NonTerminalAction non_terminal_action,
    ArchVmAspaceInterface::TerminalAction terminal_action);
zx_status_t cpp_arch_vm_aspace_mark_accessed(ArchVmAspace* aspace, vaddr_t vaddr, size_t count);
bool cpp_arch_vm_aspace_accessed_since_last_check(ArchVmAspace* aspace, bool clear);
paddr_t cpp_arch_vm_aspace_arch_table_phys(ArchVmAspace* aspace);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_ARCH_VM_ASPACE_FFI_H_
