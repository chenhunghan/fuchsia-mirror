// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <stdint.h>
#include <zircon/types.h>

#include <arch/aspace.h>
#include <arch/vm.h>
#include <kernel/ffi.h>
#include <ktl/memory.h>
#include <vm/arch_vm_aspace.h>
#include <vm/arch_vm_aspace_ffi.h>

#include <ktl/enforce.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations below once
// cross-language inlining works.

FFI_ALWAYS_INLINE void cpp_arch_vm_aspace_construct(
    ffi::Uninitialized<ArchVmAspace>* aspace, vaddr_t base, size_t size, arch_mmu_flags_t mmu_flags,
    ArchVmAspaceInterface::page_alloc_fn_t test_paf) {
  aspace->Initialize(base, size, mmu_flags, test_paf);
}
FFI_ALWAYS_INLINE void cpp_arch_vm_aspace_destruct(ArchVmAspace* aspace) {
  ktl::destroy_at(aspace);
}
FFI_ALWAYS_INLINE zx_status_t cpp_arch_vm_aspace_init(ArchVmAspace* aspace) {
  return aspace->Init();
}
FFI_ALWAYS_INLINE zx_status_t cpp_arch_vm_aspace_init_shared(ArchVmAspace* aspace) {
  return aspace->InitShared();
}
FFI_ALWAYS_INLINE zx_status_t cpp_arch_vm_aspace_init_restricted(ArchVmAspace* aspace) {
  return aspace->InitRestricted();
}
FFI_ALWAYS_INLINE zx_status_t cpp_arch_vm_aspace_init_unified(ArchVmAspace* aspace,
                                                              ArchVmAspace* shared,
                                                              ArchVmAspace* restricted) {
  return aspace->InitUnified(*shared, *restricted);
}
FFI_ALWAYS_INLINE void cpp_arch_vm_aspace_disable_updates(ArchVmAspace* aspace) {
  aspace->DisableUpdates();
}
FFI_ALWAYS_INLINE zx_status_t cpp_arch_vm_aspace_destroy(ArchVmAspace* aspace) {
  return aspace->Destroy();
}
FFI_ALWAYS_INLINE zx_status_t cpp_arch_vm_aspace_map_contiguous(ArchVmAspace* aspace, vaddr_t vaddr,
                                                                paddr_t paddr, size_t count,
                                                                arch_mmu_flags_t mmu_flags) {
  return aspace->MapContiguous(vaddr, paddr, count, mmu_flags);
}
FFI_ALWAYS_INLINE zx_status_t cpp_arch_vm_aspace_map(
    ArchVmAspace* aspace, vaddr_t vaddr, paddr_t* phys, size_t count, arch_mmu_flags_t mmu_flags,
    ArchVmAspaceInterface::ExistingEntryAction existing_action) {
  return aspace->Map(vaddr, phys, count, mmu_flags, existing_action);
}
FFI_ALWAYS_INLINE zx_status_t
cpp_arch_vm_aspace_unmap(ArchVmAspace* aspace, vaddr_t vaddr, size_t count,
                         ArchVmAspaceInterface::ArchUnmapOptions enlarge) {
  return aspace->Unmap(vaddr, count, enlarge);
}
FFI_ALWAYS_INLINE bool cpp_arch_vm_aspace_unmap_only_enlarge_on_oom(ArchVmAspace* aspace) {
  return aspace->UnmapOnlyEnlargeOnOom();
}
FFI_ALWAYS_INLINE zx_status_t cpp_arch_vm_aspace_protect(
    ArchVmAspace* aspace, vaddr_t vaddr, size_t count, arch_mmu_flags_t mmu_flags,
    ArchVmAspaceInterface::ArchUnmapOptions enlarge) {
  return aspace->Protect(vaddr, count, mmu_flags, enlarge);
}
FFI_ALWAYS_INLINE zx_status_t cpp_arch_vm_aspace_query(ArchVmAspace* aspace, vaddr_t vaddr,
                                                       paddr_t* paddr,
                                                       arch_mmu_flags_t* mmu_flags) {
  return aspace->Query(vaddr, paddr, mmu_flags);
}
FFI_ALWAYS_INLINE vaddr_t cpp_arch_vm_aspace_pick_spot(ArchVmAspace* aspace, vaddr_t base,
                                                       vaddr_t end, vaddr_t align, size_t size,
                                                       arch_mmu_flags_t mmu_flags) {
  return aspace->PickSpot(base, end, align, size, mmu_flags);
}
FFI_ALWAYS_INLINE zx_status_t
cpp_arch_vm_aspace_harvest_accessed(ArchVmAspace* aspace, vaddr_t vaddr, size_t count,
                                    ArchVmAspaceInterface::NonTerminalAction non_terminal_action,
                                    ArchVmAspaceInterface::TerminalAction terminal_action) {
  return aspace->HarvestAccessed(vaddr, count, non_terminal_action, terminal_action);
}
FFI_ALWAYS_INLINE zx_status_t cpp_arch_vm_aspace_mark_accessed(ArchVmAspace* aspace, vaddr_t vaddr,
                                                               size_t count) {
  return aspace->MarkAccessed(vaddr, count);
}
FFI_ALWAYS_INLINE bool cpp_arch_vm_aspace_accessed_since_last_check(ArchVmAspace* aspace,
                                                                    bool clear) {
  return aspace->AccessedSinceLastCheck(clear);
}
FFI_ALWAYS_INLINE paddr_t cpp_arch_vm_aspace_arch_table_phys(ArchVmAspace* aspace) {
  return aspace->arch_table_phys();
}
#if ARCH_ARM64
FFI_ALWAYS_INLINE void cpp_arch_vm_aspace_arch_set_asid(ArchVmAspace* aspace, uint16_t asid) {
  aspace->arch_set_asid(asid);
}
#endif
FFI_ALWAYS_INLINE vaddr_t cpp_arch_vm_aspace_next_user_page_table_offset(vaddr_t va) {
  return ArchVmAspace::NextUserPageTableOffset(va);
}
FFI_ALWAYS_INLINE void cpp_arch_vm_aspace_context_switch(ArchVmAspace* from, ArchVmAspace* to) {
  ArchVmAspace::ContextSwitch(from, to);
}
FFI_ALWAYS_INLINE void cpp_arch_vm_aspace_handoff_page_tables_from_physboot(
    VmPageDoublyLinkedList* mmu_pages) {
  ArchVmAspace::HandoffPageTablesFromPhysboot(mmu_pages);
}

}  // extern "C"
