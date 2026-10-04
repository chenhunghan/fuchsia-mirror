// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <align.h>
#include <assert.h>
#include <bits.h>
#include <debug.h>
#include <inttypes.h>
#include <lib/arch/riscv64/feature.h>
#include <lib/arch/riscv64/paging-traits.h>
#include <lib/arch/riscv64/system.h>
#include <lib/boot-options/boot-options.h>
#include <lib/counters.h>
#include <lib/fit/defer.h>
#include <lib/heap.h>
#include <lib/ktrace.h>
#include <lib/page/size.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <trace.h>
#include <zircon/errors.h>

#include <arch/aspace.h>
#include <arch/riscv64/mmu.h>
#include <vm/physmap.h>
#include <vm/pmm.h>

// Every `aspace` argument is the C++ object's `rust_aspace_` storage.
extern "C" {
void rust_riscv64_aspace_construct(void* aspace, size_t base, size_t size, Riscv64AspaceType type,
                                   ArchVmAspaceInterface::page_alloc_fn_t paf);
void rust_riscv64_aspace_destruct(void* aspace);
zx_status_t rust_riscv64_aspace_destroy(void* aspace);
zx_status_t rust_riscv64_aspace_map(void* aspace, size_t vaddr, const paddr_t* phys, size_t count,
                                    arch_mmu_flags_t mmu_flags,
                                    ArchVmAspaceInterface::ExistingEntryAction existing_action);
zx_status_t rust_riscv64_aspace_map_contiguous(void* aspace, size_t vaddr, paddr_t paddr,
                                               size_t count, arch_mmu_flags_t mmu_flags);
zx_status_t rust_riscv64_aspace_unmap(void* aspace, size_t vaddr, size_t count, uint8_t enlarge);
zx_status_t rust_riscv64_aspace_protect(void* aspace, size_t vaddr, size_t count,
                                        arch_mmu_flags_t mmu_flags, uint8_t enlarge);
zx_status_t rust_riscv64_aspace_harvest_accessed(void* aspace, size_t vaddr, size_t count,
                                                 ArchVmAspaceInterface::TerminalAction terminal);
zx_status_t rust_riscv64_aspace_mark_accessed(void* aspace, size_t vaddr, size_t count);
zx_status_t rust_riscv64_aspace_query(void* aspace, size_t vaddr, paddr_t* paddr,
                                      arch_mmu_flags_t* mmu_flags);
paddr_t rust_riscv64_aspace_arch_table_phys(const void* aspace);
uint16_t rust_riscv64_aspace_asid(const void* aspace);
zx_status_t rust_riscv64_aspace_init(void* aspace);
zx_status_t rust_riscv64_aspace_init_shared(void* aspace);
zx_status_t rust_riscv64_aspace_init_restricted(void* aspace);
zx_status_t rust_riscv64_aspace_init_unified(void* aspace, void* shared, void* restricted);
void rust_riscv64_aspace_context_switch(void* old, void* new_aspace);
bool rust_riscv64_aspace_accessed_since_last_check(void* aspace, bool clear);
size_t rust_riscv64_aspace_pick_spot(void* aspace, size_t base, size_t end, size_t align,
                                     size_t size, arch_mmu_flags_t mmu_flags);
Riscv64AspaceType rust_riscv64_aspace_type_from_flags(arch_mmu_flags_t mmu_flags);

void rust_riscv64_mmu_early_init();
void rust_riscv64_mmu_early_init_percpu();
void rust_riscv64_mmu_prevm_init();
void rust_riscv64_mmu_init();
void rust_riscv64_icache_sync_addr(vaddr_t start, size_t len);
void rust_riscv64_icache_finish();
paddr_t rust_riscv64_get_bootstrap_translation_table();
}  // extern "C"

// The Rust side mirrors these as `#[repr(u8)]` enums and they are passed across the
// FFI boundary unconverted, so their widths are part of the ABI.  A scoped enum with
// no fixed underlying type would be `int`.
static_assert(sizeof(Riscv64AspaceType) == 1);
static_assert(sizeof(ArchVmAspaceInterface::ExistingEntryAction) == 1);
static_assert(sizeof(ArchVmAspaceInterface::TerminalAction) == 1);

Riscv64ArchVmAspace::Riscv64ArchVmAspace(vaddr_t base, size_t size, Riscv64AspaceType type,
                                         page_alloc_fn_t test_paf) {
  rust_riscv64_aspace_construct(&rust_aspace_, base, size, type, test_paf);
}
Riscv64ArchVmAspace::Riscv64ArchVmAspace(vaddr_t base, size_t size, arch_mmu_flags_t mmu_flags,
                                         page_alloc_fn_t test_paf) {
  rust_riscv64_aspace_construct(&rust_aspace_, base, size,
                                rust_riscv64_aspace_type_from_flags(mmu_flags), test_paf);
}

Riscv64ArchVmAspace::~Riscv64ArchVmAspace() { rust_riscv64_aspace_destruct(&rust_aspace_); }

zx_status_t Riscv64ArchVmAspace::InitShared() {
  return rust_riscv64_aspace_init_shared(&rust_aspace_);
}

zx_status_t Riscv64ArchVmAspace::InitRestricted() {
  return rust_riscv64_aspace_init_restricted(&rust_aspace_);
}

zx_status_t Riscv64ArchVmAspace::InitUnified(ArchVmAspaceInterface& shared,
                                             ArchVmAspaceInterface& restricted) {
  auto& shared_aspace = static_cast<Riscv64ArchVmAspace&>(shared);
  auto& restricted_aspace = static_cast<Riscv64ArchVmAspace&>(restricted);
  return rust_riscv64_aspace_init_unified(&rust_aspace_, &shared_aspace.rust_aspace_,
                                          &restricted_aspace.rust_aspace_);
}

zx_status_t Riscv64ArchVmAspace::Destroy() {
  // Tears down the page tables and releases the ASID.  The Rust aspace object
  // itself is dropped by the destructor.
  return rust_riscv64_aspace_destroy(&rust_aspace_);
}

zx_status_t Riscv64ArchVmAspace::Map(vaddr_t vaddr, paddr_t* phys, size_t count,
                                     arch_mmu_flags_t mmu_flags,
                                     ExistingEntryAction existing_action) {
  return rust_riscv64_aspace_map(&rust_aspace_, vaddr, phys, count, mmu_flags, existing_action);
}

zx_status_t Riscv64ArchVmAspace::MapContiguous(vaddr_t vaddr, paddr_t paddr, size_t count,
                                               arch_mmu_flags_t mmu_flags) {
  return rust_riscv64_aspace_map_contiguous(&rust_aspace_, vaddr, paddr, count, mmu_flags);
}

zx_status_t Riscv64ArchVmAspace::Unmap(vaddr_t vaddr, size_t count, ArchUnmapOptions enlarge) {
  return rust_riscv64_aspace_unmap(&rust_aspace_, vaddr, count, static_cast<uint8_t>(enlarge));
}

zx_status_t Riscv64ArchVmAspace::Protect(vaddr_t vaddr, size_t count, arch_mmu_flags_t mmu_flags,
                                         ArchUnmapOptions enlarge) {
  return rust_riscv64_aspace_protect(&rust_aspace_, vaddr, count, mmu_flags,
                                     static_cast<uint8_t>(enlarge));
}

zx_status_t Riscv64ArchVmAspace::Init() { return rust_riscv64_aspace_init(&rust_aspace_); }

void Riscv64ArchVmAspace::DisableUpdates() {
  // TODO-rvbringup: add machinery for this and the update checker logic
}

zx_status_t Riscv64ArchVmAspace::HarvestAccessed(vaddr_t vaddr, size_t count,
                                                 NonTerminalAction non_terminal_action,
                                                 TerminalAction terminal_action) {
  return rust_riscv64_aspace_harvest_accessed(&rust_aspace_, vaddr, count, terminal_action);
}

zx_status_t Riscv64ArchVmAspace::MarkAccessed(vaddr_t vaddr, size_t count) {
  return rust_riscv64_aspace_mark_accessed(&rust_aspace_, vaddr, count);
}

zx_status_t Riscv64ArchVmAspace::Query(vaddr_t vaddr, paddr_t* paddr, arch_mmu_flags_t* mmu_flags) {
  return rust_riscv64_aspace_query(&rust_aspace_, vaddr, paddr, mmu_flags);
}

vaddr_t Riscv64ArchVmAspace::PickSpot(vaddr_t base, vaddr_t end, vaddr_t align, size_t size,
                                      arch_mmu_flags_t mmu_flags) {
  return rust_riscv64_aspace_pick_spot(&rust_aspace_, base, end, align, size, mmu_flags);
}

paddr_t Riscv64ArchVmAspace::arch_table_phys() const {
  return rust_riscv64_aspace_arch_table_phys(&rust_aspace_);
}

uint16_t Riscv64ArchVmAspace::asid() const { return rust_riscv64_aspace_asid(&rust_aspace_); }

void Riscv64ArchVmAspace::ContextSwitch(Riscv64ArchVmAspace* old_aspace,
                                        Riscv64ArchVmAspace* aspace) {
  rust_riscv64_aspace_context_switch(old_aspace ? &old_aspace->rust_aspace_ : nullptr,
                                     aspace ? &aspace->rust_aspace_ : nullptr);
}

bool Riscv64ArchVmAspace::AccessedSinceLastCheck(bool clear) {
  return rust_riscv64_aspace_accessed_since_last_check(&rust_aspace_, clear);
}

void Riscv64ArchVmAspace::HandoffPageTablesFromPhysboot(VmPageDoublyLinkedList* mmu_pages) {
  // This must drain |mmu_pages|: PmmNode::InitReservedRange() destroys the list
  // immediately afterwards and it is an error to destroy a non-empty list of
  // unmanaged pointers.
  while (vm_page_t* page = mmu_pages->pop_front()) {
    page->set_state(vm_page_state::MMU);

    ktl::span entries{
        reinterpret_cast<pte_t*>(paddr_to_physmap(page->paddr())),
        kPageSize / sizeof(pte_t),
    };
    page->mmu.num_mappings = 0;
    for (pte_t entry : entries) {
      if ((entry & RISCV64_PTE_V) != 0) {
        page->mmu.num_mappings++;
      }
    }
    page->set_state(vm_page_state::MMU);
  }
}

void riscv64_mmu_early_init() { rust_riscv64_mmu_early_init(); }
void riscv64_mmu_early_init_percpu() { rust_riscv64_mmu_early_init_percpu(); }
void riscv64_mmu_prevm_init() { rust_riscv64_mmu_prevm_init(); }
void riscv64_mmu_init() { rust_riscv64_mmu_init(); }

paddr_t riscv64_get_bootstrap_translation_table() {
  return rust_riscv64_get_bootstrap_translation_table();
}

void Riscv64VmICacheConsistencyManager::SyncAddr(vaddr_t start, size_t len) {
  rust_riscv64_icache_sync_addr(start, len);
  need_invalidate_ = true;
}

void Riscv64VmICacheConsistencyManager::Finish() {
  if (need_invalidate_) {
    rust_riscv64_icache_finish();
    need_invalidate_ = false;
  }
}
