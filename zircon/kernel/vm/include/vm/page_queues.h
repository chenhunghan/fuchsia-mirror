// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_QUEUES_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_QUEUES_H_

#include <sys/types.h>

#include <fbl/macros.h>
#include <kernel/ffi.h>
#include <vm/page.h>

class VmCowPages;
class PageQueues;

extern "C" {
void rust_page_queues_move_to_reclaim_dont_need(PageQueues* queues, vm_page_t* page);

void rust_page_queues_compress_failed(PageQueues* queues, vm_page_t* page);

void rust_page_queues_change_object_offset_array(PageQueues* queues, vm_page_t** pages,
                                                 VmCowPages* object, const uint64_t* offsets,
                                                 size_t count);

void rust_page_queues_remove(PageQueues* queues, vm_page_t* page);
void rust_page_queues_remove_array_into_list(PageQueues* queues, vm_page_t** pages, size_t count,
                                             VmPageDoublyLinkedList* out_list);

void rust_page_queues_mark_accessed(PageQueues* queues, vm_page_t* page);

bool rust_page_queues_is_page_reclaimable(const vm_page_t* page);

bool rust_page_queues_debug_page_is_pager_backed_dirty(const PageQueues* queues,
                                                       const vm_page_t* page);
}  // extern "C"

// Allocated pages that are part of the cow pages in a VmObjectPaged can be placed in a page queue.
// The page queues provide a way to
//  * Classify and group pages across VMO boundaries
//  * Retrieve the VMO that a page is contained in (via a back reference stored in the vm_page_t)
// Once a page has been placed in a page queue its queue_node becomes owned by the page queue and
// must not be used until the page has been Remove'd. It is not sufficient to call list_delete on
// the queue_node yourself as this operation is not atomic and needs to be performed whilst holding
// the PageQueues::lock_.
//
// This is an opaque handle to a PageQueues that is implemented, and owned, by Rust (see
// page_queues.rs). It has no state of its own, cannot be constructed from C++, and forwards all of
// its methods to Rust.
class PageQueues {
 public:
  PageQueues() = delete;
  ~PageQueues() = delete;

  DISALLOW_COPY_ASSIGN_AND_MOVE(PageQueues);

  // All Move operations change the queue that a page is considered to be in, but do not change the
  // object or offset backlink information. The page must currently be in a valid page queue.

  // TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
  FFI_ALWAYS_INLINE void MoveToReclaimDontNeed(vm_page_t* page) {
    rust_page_queues_move_to_reclaim_dont_need(this, page);
  }

  // Indicates that page has failed a compression attempted, and moves it to a separate queue to
  // prevent it from being considered part of the reclaim set, which makes it neither active nor
  // inactive. The specified page must be in the page queues, but if not presently in a reclaim
  // queue this method will do nothing.
  // TODO(https://fxbug.dev/42138396): Determine whether/how pages are moved back into the reclaim
  // pool and either further generalize this to support pager backed, or specialize FailedReclaim to
  // be explicitly only anonymous.
  FFI_ALWAYS_INLINE void CompressFailed(vm_page_t* page) {
    rust_page_queues_compress_failed(this, page);
  }

  // Changes the backlink information for a page and should only be called by the page owner under
  // its lock (that is the VMO lock). The page must currently be in a valid page queue.
  FFI_ALWAYS_INLINE void ChangeObjectOffsetArray(vm_page_t** pages, VmCowPages* object,
                                                 uint64_t* offsets, size_t count) {
    rust_page_queues_change_object_offset_array(this, pages, object, offsets, count);
  }

  // Removes the page from any page list and returns ownership of the queue_node.
  FFI_ALWAYS_INLINE void Remove(vm_page_t* page) { rust_page_queues_remove(this, page); }
  // Batched version of Remove that also places all the pages in the specified list
  FFI_ALWAYS_INLINE void RemoveArrayIntoList(vm_page_t** page, size_t count,
                                             VmPageDoublyLinkedList* out_list) {
    rust_page_queues_remove_array_into_list(this, page, count, out_list);
  }

  // Tells the page queue this page has been accessed, and it should have its position in the queues
  // updated.
  FFI_ALWAYS_INLINE void MarkAccessed(vm_page_t* page) {
    rust_page_queues_mark_accessed(this, page);
  }

  // Returns true if the page is in an isolate queue.
  FFI_ALWAYS_INLINE static bool IsPageReclaimable(const vm_page_t* page) {
    return rust_page_queues_is_page_reclaimable(page);
  }

  // These query functions are marked Debug as it is generally a racy way to determine a pages state
  // and these are exposed for the purpose of writing tests or asserts against the pagequeue.
  FFI_ALWAYS_INLINE bool DebugPageIsPagerBackedDirty(const vm_page_t* page) const {
    return rust_page_queues_debug_page_is_pager_backed_dirty(this, page);
  }
};

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_QUEUES_H_
