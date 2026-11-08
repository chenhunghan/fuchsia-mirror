// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/pmm.h"

#include <lib/ktrace.h>

#include <vm/pmm_node.h>
#include <vm/vm.h>

#include <ktl/enforce.h>

zx_status_t pmm_alloc_page(uint alloc_flags, vm_page_t** page) {
  VM_KTRACE_DURATION(3, "pmm_alloc_page", ("count", 1), ("alloc_flags", alloc_flags));
  zx::result<vm_page_t*> result = Pmm::Node().AllocPage(alloc_flags);
  if (result.is_ok()) {
    *page = result.value();
  }
  return result.status_value();
}

zx_status_t pmm_alloc_page(uint alloc_flags, vm_page_t** page, paddr_t* pa) {
  VM_KTRACE_DURATION(3, "pmm_alloc_page", ("count", 1), ("alloc_flags", alloc_flags));
  zx::result<vm_page_t*> result = Pmm::Node().AllocPage(alloc_flags);
  if (result.is_ok()) {
    *page = result.value();
    *pa = (*page)->paddr();
  }
  return result.status_value();
}

void pmm_free(VmPageDoublyLinkedList* list, PmmOptDelayReuse delay_reuse) {
  VM_KTRACE_DURATION(3, "pmm_free");
  Pmm::Node().FreeList(list, delay_reuse);
}

void pmm_free_page(vm_page* page, PmmOptDelayReuse delay_reuse) {
  VM_KTRACE_DURATION(3, "pmm_free_page");
  Pmm::Node().FreePage(page, delay_reuse);
}
