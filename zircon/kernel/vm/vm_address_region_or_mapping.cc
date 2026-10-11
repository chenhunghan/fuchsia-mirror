// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#include <assert.h>
#include <inttypes.h>
#include <string.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <vm/vm.h>
#include <vm/vm_aspace.h>

#include "vm/vm_address_region.h"
#include "vm_priv.h"

#define LOCAL_TRACE VM_GLOBAL_TRACE(0)

VmAddressRegionOrMapping::VmAddressRegionOrMapping(vaddr_t base, size_t size, uint32_t flags,
                                                   VmAspace* aspace, VmAddressRegion* parent,
                                                   bool is_mapping)
    : is_mapping_(is_mapping),
      state_(LifeCycleState::NOT_READY),
      flags_(flags),
      base_(base),
      size_(size),
      aspace_(aspace),
      parent_(parent) {
  LTRACEF("%p\n", this);
}

zx_status_t VmAddressRegionOrMapping::Destroy() {
  canary_.Assert();

  Guard<CriticalMutex> region_guard{aspace_->region_lock()};
  Guard<CriticalMutex> guard{aspace_->lock()};
  if (state_ != LifeCycleState::ALIVE) {
    return ZX_ERR_BAD_STATE;
  }

  return DestroyLocked();
}

VmAddressRegionOrMapping::~VmAddressRegionOrMapping() {
  LTRACEF("%p\n", this);

  ASSERT(state_ != LifeCycleState::ALIVE);

  DEBUG_ASSERT(memory_priority_ == MemoryPriority::DEFAULT);
}

zx_status_t VmAddressRegionOrMapping::DestroyLocked() {
  if (is_mapping_) {
    return static_cast<VmMapping*>(this)->DestroyLockedImpl();
  }
  return static_cast<VmAddressRegion*>(this)->DestroyLockedImpl();
}

zx_status_t VmAddressRegionOrMapping::Activate() {
  if (is_mapping_) {
    return static_cast<VmMapping*>(this)->ActivateImpl();
  }
  return static_cast<VmAddressRegion*>(this)->ActivateImpl();
}

void VmAddressRegionOrMapping::DumpLocked(uint depth, bool verbose) const {
  if (is_mapping_) {
    static_cast<const VmMapping*>(this)->DumpLockedImpl(depth, verbose);
  } else {
    static_cast<const VmAddressRegion*>(this)->DumpLockedImpl(depth, verbose);
  }
}

void VmAddressRegionOrMapping::CommitHighMemoryPriority() {
  if (is_mapping_) {
    static_cast<VmMapping*>(this)->CommitHighMemoryPriorityImpl();
  } else {
    static_cast<VmAddressRegion*>(this)->CommitHighMemoryPriorityImpl();
  }
}

void VmAddressRegionOrMapping::fbl_recycle() {
  if (is_mapping_) {
    delete static_cast<VmMapping*>(this);
  } else {
    delete static_cast<VmAddressRegion*>(this);
  }
}
