// Copyright 2017 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_H_

#include <lib/memalloc/range.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <ktl/span.h>
#include <vm/page.h>
#include <vm/pmm_arena.h>
#include <vm/pmm_node.h>

// The following functions are implemented in Rust (see pmm.rs).
__BEGIN_CDECLS

// Returns the global PmmNode. See Pmm::Node(). The returned PmmNode is not constructed until the
// Rust `pmm_node_init` LK init hook has run at LK_INIT_LEVEL_PLATFORM_EARLY - 1.
PmmNode* rust_pmm_node();

// See pmm_init().
zx_status_t rust_pmm_init(const memalloc::Range* ranges, size_t count);

// Ends the PMM's role within the context of phys handoff: it frees all physical
// memory temporarily used for the hand-off from physboot. Since this memory
// includes that backing the hand-off struct itself (accessible via
// gPhysHandoff), this call is intended to be the last thing done in the process
// of ending the hand-off.
void pmm_end_handoff();

// Returns the number of arenas.
size_t pmm_num_arenas();

// Copies |count| pmm_arena_info_t objects into |buffer| starting with the |i|-th arena ordered by
// base address.  For example, passing an |i| of 1 would skip the 1st arena.
//
// The objects will be sorted in ascending order by arena base address.
//
// Returns ZX_ERR_OUT_OF_RANGE if |count| is 0 or |i| and |count| specify an invalid range.
//
// Returns ZX_ERR_BUFFER_TOO_SMALL if the buffer is too small.
zx_status_t pmm_get_arena_info(size_t count, uint64_t i, pmm_arena_info_t* buffer,
                               size_t buffer_size);

// Allocate count pages of physical memory, adding to the tail of the passed list.
// The list must be initialized.
// Note that if PMM_ALLOC_FLAG_CAN_WAIT is passed in then this could always return
// ZX_ERR_SHOULD_WAIT. Since there is no way to wait until an arbitrary number of pages can be
// allocated (see comment on |pmm_wait_till_should_retry_single_alloc|) passing
// PMM_ALLOC_FLAG_CAN_WAIT here should be used as an optimistic fast path, and the caller should
// have a fallback of allocating single pages.
zx_status_t pmm_alloc_pages(size_t count, uint32_t alloc_flags, VmPageDoublyLinkedList* list)
    __NONNULL((3));

// Allocate a run of contiguous pages, aligned on log2 byte boundary (0-31).
// Return the base address of the run in the physical address pointer and
// append the allocate page structures to the tail of the passed in list.
zx_status_t pmm_alloc_contiguous(size_t count, uint32_t alloc_flags, uint8_t align_log2,
                                 paddr_t* pa, VmPageDoublyLinkedList* list) __NONNULL((4, 5));

// Unwires a page and sets it in the ALLOC state.
void pmm_unwire_page(vm_page_t* page) __NONNULL((1));

// Should be called after the kernel command line has been parsed.
//
// Must only be called during early boot, before the kernel console can access the PMM checker.
void rust_pmm_checker_init_from_cmdline();

// Synchronously walk the PMM's free list and poison (via kASAN) each page. This is an
// incredibly expensive operation and should be used with care.
//
// Only defined in kASAN builds. This is not guarded by __has_feature(address_sanitizer) since the
// caller in the kASAN runtime is itself compiled without ASan instrumentation.
void pmm_asan_poison_all_free_pages();

__END_CDECLS

// Pmm class exists purely to serve as a way to provide a public accessor of the global PmmNode,
// which is owned by Rust.
class Pmm {
 public:
  // Retrieve the global PmmNode. Must not be called before the Rust `pmm_node_init` LK init hook
  // has run, which happens just before platform_early_init().
  static PmmNode& Node() { return *rust_pmm_node(); }
};

// Initializes the PMM with the provided normalized memory ranges. This in
// particular initializes its arenas and wires any previously allocated special
// subranges or holes.
inline zx_status_t pmm_init(ktl::span<const memalloc::Range> ranges) {
  return rust_pmm_init(ranges.data(), ranges.size());
}

// Allocate a single page of physical memory.
zx_status_t pmm_alloc_page(uint alloc_flags, vm_page** p) __NONNULL((2));
zx_status_t pmm_alloc_page(uint alloc_flags, vm_page** p, paddr_t* pa) __NONNULL((2, 3));

// Free a list of physical pages. This list must not contained loaned pages returned from
// PmmNode::AllocLoanedPage.
//
// |delay_reuse| specifies whether reuse of the pages (i.e. reallocation) should be delayed or not.
void pmm_free(VmPageDoublyLinkedList* list,
              PmmOptDelayReuse delay_reuse = PmmOptDelayReuse::Default) __NONNULL((1));

// Free a single page. This page must not be a loaned page returned from PmmNode::AllocLoanedPage.
//
// See |pmm_free| for meaning of the |delay_reuse| parameter.
void pmm_free_page(vm_page_t* page, PmmOptDelayReuse delay_reuse = PmmOptDelayReuse::Default)
    __NONNULL((1));

// Return the PageQueues.
inline PageQueues* pmm_page_queues() { return Pmm::Node().GetPageQueues(); }

// virtual to physical for kernel addresses.
paddr_t vaddr_to_paddr(const void* va);

// paddr to vm_page_t
inline vm_page_t* paddr_to_vm_page(paddr_t addr) { return Pmm::Node().PaddrToPage(addr); }

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_H_
