// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/pmm_node.h"

#include <vm/pmm_node_ffi.h>

vm_page_t* PmmNode::PaddrToPage(paddr_t addr) { return rust_pmm_node_paddr_to_page(this, addr); }

vm_page_t* PmmNode::IndexToPage(uint32_t index) { return rust_pmm_node_index_to_page(this, index); }

paddr_t PmmNode::IndexToPaddr(uint32_t index) { return rust_pmm_node_index_to_paddr(this, index); }

PageQueues* PmmNode::GetPageQueues() { return rust_pmm_node_get_page_queues(this); }

VmCompression* PmmNode::GetPageCompression() { return rust_pmm_node_get_page_compression(this); }

zx::result<vm_page_t*> PmmNode::AllocLoanedPage(
    fit::inline_function<void(vm_page_t*), 32> allocated) {
  vm_page_t* out_page = nullptr;
  auto cb = [](vm_page_t* page, void* cookie) {
    auto* fn = reinterpret_cast<fit::inline_function<void(vm_page_t*), 32>*>(cookie);
    (*fn)(page);
  };
  zx_status_t status = rust_pmm_node_alloc_loaned_page(this, cb, &allocated, &out_page);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(out_page);
}

zx::result<vm_page_t*> PmmNode::AllocPage(uint alloc_flags) {
  vm_page_t* page = nullptr;
  zx_status_t status = rust_pmm_node_alloc_page(this, alloc_flags, &page);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(page);
}

void PmmNode::BeginFreeLoanedPage(vm_page_t* page,
                                  fit::inline_function<void(vm_page_t*)> release_page,
                                  FreeLoanedPagesHolder& flph) {
  auto cb = [](vm_page_t* page, void* cookie) {
    auto* fn = reinterpret_cast<fit::inline_function<void(vm_page_t*)>*>(cookie);
    (*fn)(page);
  };
  rust_pmm_node_begin_free_loaned_page(this, page, cb, &release_page, &flph);
}

void PmmNode::FinishFreeLoanedPages(FreeLoanedPagesHolder& flph) {
  rust_pmm_node_finish_free_loaned_pages(this, &flph);
}

void PmmNode::FreePage(vm_page* page, PmmOptDelayReuse delay_reuse) {
  rust_pmm_node_free_page(this, page, delay_reuse);
}

void PmmNode::BeginFreeLoanedArray(
    vm_page_t** pages, size_t count,
    fit::inline_function<void(vm_page_t**, size_t, VmPageDoublyLinkedList*)> release_list,
    FreeLoanedPagesHolder& flph) {
  auto cb = [](vm_page_t** pages, size_t count, VmPageDoublyLinkedList* list, void* cookie) {
    auto* fn =
        reinterpret_cast<fit::inline_function<void(vm_page_t**, size_t, VmPageDoublyLinkedList*)>*>(
            cookie);
    (*fn)(pages, count, list);
  };
  rust_pmm_node_begin_free_loaned_array(this, pages, count, cb, &release_list, &flph);
}

void PmmNode::FreeList(VmPageDoublyLinkedList* list, PmmOptDelayReuse delay_reuse) {
  rust_pmm_node_free_list(this, list, delay_reuse);
}
