// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_NODE_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_NODE_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include "vm/pmm_node.h"

__BEGIN_CDECLS

vm_page_t* rust_pmm_node_paddr_to_page(PmmNode* node, paddr_t addr);
vm_page_t* rust_pmm_node_index_to_page(PmmNode* node, uint32_t index);
paddr_t rust_pmm_node_index_to_paddr(PmmNode* node, uint32_t index);
PageQueues* rust_pmm_node_get_page_queues(PmmNode* node);
VmCompression* rust_pmm_node_get_page_compression(PmmNode* node);
zx_status_t rust_pmm_node_alloc_page(PmmNode* node, uint32_t alloc_flags, vm_page_t** out_page);
void rust_pmm_node_free_page(PmmNode* node, vm_page_t* page, PmmOptDelayReuse delay_reuse);
void rust_pmm_node_free_list(PmmNode* node, VmPageDoublyLinkedList* list,
                             PmmOptDelayReuse delay_reuse);
zx_status_t rust_pmm_node_alloc_loaned_page(PmmNode* node,
                                            void (*allocated)(vm_page_t*, void* cookie),
                                            void* cookie, vm_page_t** out_page);
void rust_pmm_node_begin_free_loaned_page(PmmNode* node, vm_page_t* page,
                                          void (*release_page)(vm_page_t*, void* cookie),
                                          void* cookie, FreeLoanedPagesHolder* flph);
void rust_pmm_node_finish_free_loaned_pages(PmmNode* node, FreeLoanedPagesHolder* flph);
void rust_pmm_node_begin_free_loaned_array(PmmNode* node, vm_page_t** pages, size_t count,
                                           void (*release_list)(vm_page_t**, size_t,
                                                                VmPageDoublyLinkedList*,
                                                                void* cookie),
                                           void* cookie, FreeLoanedPagesHolder* flph);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_NODE_FFI_H_
