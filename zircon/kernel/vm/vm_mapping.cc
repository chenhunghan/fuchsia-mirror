// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <inttypes.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <ktl/utility.h>
#include <vm/vm_address_region.h>
#include <vm/vm_object.h>

#include "vm_priv.h"

#include <ktl/enforce.h>

#define LOCAL_TRACE VM_GLOBAL_TRACE(0)

VmMapping::VmMapping(VmAddressRegion& parent, bool private_clone, vaddr_t base, size_t size,
                     uint32_t vmar_flags, fbl::RefPtr<VmObject> vmo, uint64_t vmo_offset,
                     arch_mmu_flags_t first_mmu_flags,
                     btree::BTree<vaddr_t, arch_mmu_flags_t>&& ranges, Mergeable mergeable)
    : VmAddressRegionOrMapping(base, size, vmar_flags, parent.aspace().get(), &parent, true),
      rest_protection_ranges_(ktl::move(ranges)) {
  rust_vm_mapping_state_init(&opaque_storage_, fbl::ExportToRawPtr(&vmo), vmo_offset,
                             first_mmu_flags, private_clone, mergeable);
  LTRACEF("%p aspace %p base %#" PRIxPTR " size %#zx offset %#" PRIx64 "\n", this, aspace().get(),
          this->base(), this->size(), vmo_offset);
}

VmMapping::VmMapping(VmAddressRegion& parent, bool private_clone, vaddr_t base, size_t size,
                     uint32_t vmar_flags, fbl::RefPtr<VmObject> vmo, uint64_t vmo_offset,
                     arch_mmu_flags_t arch_mmu_flags, Mergeable mergeable)
    : VmMapping(parent, private_clone, base, size, vmar_flags, ktl::move(vmo), vmo_offset,
                arch_mmu_flags, btree::BTree<vaddr_t, arch_mmu_flags_t>(), mergeable) {}

VmMapping::~VmMapping() { rust_vm_mapping_destruct(this); }

void VmMapping::fbl_recycle() { rust_vm_mapping_recycle(this); }

fbl::RefPtr<VmObject> VmMapping::vmo() const {
  return fbl::ImportFromRawPtr(rust_vm_mapping_vmo(this));
}

VmMapping::AttributionCounts VmMapping::GetAttributedMemory() const {
  AttributionCounts counts;
  rust_vm_mapping_get_attributed_memory(this, &counts);
  return counts;
}

zx_status_t VmMapping::DecommitRange(size_t offset, size_t len) {
  return rust_vm_mapping_decommit_range(this, offset, len);
}

void VmMapping::SetMemoryPriorityLocked(VmAddressRegion::MemoryPriority priority) {
  rust_vm_mapping_set_mem_priority_locked(this, priority);
}

zx::result<fbl::RefPtr<VmMapping>> VmMapping::ForceWritable() {
  VmMapping* writable = nullptr;
  zx_status_t status = rust_vm_mapping_force_writable(this, &writable);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(fbl::ImportFromRawPtr(writable));
}
