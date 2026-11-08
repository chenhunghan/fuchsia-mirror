// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_NODE_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_NODE_H_

#include <lib/page/size.h>

#include <fbl/canary.h>
#include <fbl/intrusive_double_list.h>
#include <kernel/event.h>
#include <kernel/lockdep.h>
#include <kernel/mutex.h>
#include <ktl/span.h>
#include <vm/evictor.h>
#include <vm/page_queues.h>
#include <vm/pmm_arena.h>
#include <vm/pmm_checker.h>

// flags for allocation routines below
#define PMM_ALLOC_FLAG_ANY (0 << 0)  // no restrictions on which arena to allocate from
// The caller is able to wait and retry this allocation and so pmm allocation functions are allowed
// to return ZX_ERR_SHOULD_WAIT, as opposed to ZX_ERR_NO_MEMORY, to indicate that the caller should
// wait and try again. This is intended for the PMM to tell callers who are able to wait that memory
// is low. The caller should not infer anything about memory state if it is told to wait, as the PMM
// may tell it to wait for any reason.
#define PMM_ALLOC_FLAG_CAN_WAIT (1 << 0)

// This enum is used to specify whether a page, when freed, should have its reuse
// (i.e. reallocation) delayed.  This feature exists to both improve the PMM checker's ability to
// detect "bad DMAs" and to reduce the impact when they do occur.  When allocating a page, reusing
// the most recently freed page often has performance benefits.  However, it can amplify the impact
// of use-after-free bugs.  This feature enables part of the kernel to express a preference on
// whether a page should be eligible for immediate reuse or not.  It's a hint.
//
// |Default| means no preference.  When specified, the page may or may not be immediately reused.
// In some build/runtime configurations (e.g. kasan) delayed reuse is the default behavior.
//
// |Yes| indicates that the PMM should delay the reuse of the page by placing it on the "cold" end
// up the free list, thereby maximizing the amount of time before which it is reallocated.
enum class PmmOptDelayReuse : bool { Default, Yes };

class FreeLoanedPagesHolder;
class PmmNode;
class VmCompression;

// Implemented in Rust (see pmm_node.rs). Declared here, rather than in <vm/pmm_node_ffi.h>, so that
// PmmNode::PageToIndex() can be defined inline.
__BEGIN_CDECLS
uint32_t rust_pmm_node_page_to_index(PmmNode* node, const vm_page_t* page);
__END_CDECLS

// Per numa node collection of pmm arenas and worker threads.
//
// This is an opaque handle to a PmmNode that is implemented, and owned, by Rust (see pmm_node.rs).
// It has no state of its own, cannot be constructed from C++, and forwards all of its methods to
// Rust. The global instance is retrieved with Pmm::Node().
class PmmNode {
 public:
  PmmNode() = delete;
  ~PmmNode() = delete;

  DISALLOW_COPY_ASSIGN_AND_MOVE(PmmNode);

  vm_page_t* PaddrToPage(paddr_t addr);

  static constexpr int kIndexZeroBits = 3;

  // Returns compressed representation a page_t*, with the following characteristics:
  // - zeros in the last kIndexZeroBits bits, used by clients to store metadata.
  // - The value 0 is never returned, it can be used as "no page" marker.
  // |page| must not be null. If |page| is not part of any arena then 0 is returned.
  //
  // Note: This method needs to traverse (up to) all the memory pools so it's cost is
  //       low but not trivial.
  uint32_t PageToIndex(const vm_page_t* page) {
    // This is on the hot path of encoding pages into a VmPageOrMarker, so force the Rust
    // implementation to be inlined (via LTO) into the caller.
#ifdef __clang__
    [[clang::always_inline]]
#endif
    return rust_pmm_node_page_to_index(this, page);
  }
  // Converts the number returned by PageToIndex() back to a page_t* pointer.
  // |index| must be a non-zero value previously returned by PageToIndex(). Invalid indexes, such
  // as 0, are not checked for.
  //
  // Note: This method is faster than PageToIndex(), about the cost of some basic math
  //       and bit manipulation.
  vm_page_t* IndexToPage(uint32_t index);

  // Converts the number returned by PageToIndex() back to a paddr_t.
  // |index| must be a non-zero value previously returned by PageToIndex(). Invalid indexes, such
  // as 0 or kIndexReserved0, are not checked for.
  //
  // Note: This method is faster than IndexToPage()->paddr() as the vm_page_t itself does not have
  //       to be de-referenced, saving a memory load.
  paddr_t IndexToPaddr(uint32_t index);

  // main allocator routines
  zx::result<vm_page_t*> AllocPage(uint alloc_flags);
  void FreePage(vm_page* page, PmmOptDelayReuse delay_reuse = PmmOptDelayReuse::Default);
  void FreeList(VmPageDoublyLinkedList* list,
                PmmOptDelayReuse delay_reuse = PmmOptDelayReuse::Default);

  // Allocates a single page from the loaned pages list. The allocated page will always have
  // is_loaned() being true, and must be returned by either FreeLoanedPage or FreeLoanedList. If
  // there are not loaned pages available ZX_ERR_UNAVAILABLE is returned, as an absence of loaned
  // pages does not constitute an out of memory scenario.
  // The provided callback must transition the page into a state such that it has a valid backlink,
  // i.e. it is in the OBJECT state with an owner set, prior to returning.
  // During the execution of the callback the page contents must *not* be modified.
  zx::result<vm_page_t*> AllocLoanedPage(fit::inline_function<void(vm_page_t*), 32> allocated);

  // Begins freeing a loaned page that was previously allocated by AllocLoanPage by moving into a
  // holding object. It is an error to attempt to free a non loaned page. When this method is called
  // the |page| must have a valid backlink (i.e. be in the OBJECT state with an owner set). This
  // backlink should be removed by the |release_page| callback, which is invoked under the loaned
  // pages lock, prior to transition the page into the holding state. The caller *must*, at some
  // point in the future, complete the page freeing process by passing the provided |flph| into a
  // |FinishFreeLoanedPages| call.
  void BeginFreeLoanedPage(vm_page_t* page, fit::inline_function<void(vm_page_t*)> release_page,
                           FreeLoanedPagesHolder& flph);
  // Completes the freeing of any loaned pages in |flph|, after which |flph| is allowed to be
  // destructed. Once this method is called on a given |flph| that object is effectively 'dead' and
  // is not allowed to be passed to any PmmNode methods.
  void FinishFreeLoanedPages(FreeLoanedPagesHolder& flph);

  // Begins freeing multiple pages that were allocated by AllocLoanedPage by moving into a holding
  // object. It is an error to attempt to free any non loaned pages. When this method is called all
  // pages in the array must have a valid backlink (i.e. be in the OBJECT state with an owner set),
  // and the |release_list| method must remove the backlink from all pages, and place them in the
  // provided list in the same order.
  // The caller *must*, at some point in the future, complete the page freeing process by passing
  // the provided |flph| into a |FinishFreeLoanedPages| call.
  void BeginFreeLoanedArray(
      vm_page_t** pages, size_t count,
      fit::inline_function<void(vm_page_t**, size_t, VmPageDoublyLinkedList*)> release_list,
      FreeLoanedPagesHolder& flph);

  PageQueues* GetPageQueues();

  // Retrieve any page compression instance. If this returns non-null then it's return value will
  // not change and the result can be cached.
  VmCompression* GetPageCompression();
};

// Object for managing freeing of loaned pages via a temporary holding object. Can be instantiated
// on the stack and then passed into different PmmNode methods, the object itself has no publicly
// available methods.
// This object is not thread safe, and multiple threads must not pass the same instance of this
// object into PmmNode methods.
// A given FreeLoanedPagesHolder, as described in |FinishFreeLoanedPages|, may only be used for a
// single call to |FinishFreeLoanedPages|, after which it is 'dead', and may not be passed to any
// other PmmNode methods.
class FreeLoanedPagesHolder {
 public:
  FreeLoanedPagesHolder() = default;
  ~FreeLoanedPagesHolder() {
    ASSERT(pages_.is_empty());
    ASSERT(waiters_.is_empty());
  }

 private:
  // A given FreeLoanedPagesHolder interval is only allowed to be used once to return pages to the
  // PMM, this tracks whether this has happened or not.
  // Only permitting a single instance of freeing simplifies any need to reason about a single
  // FreeLoanedPagesHolder repeatedly having pages moved into it and free'd to the PMM concurrently
  // with attempts to wait on it.
  // Although the lock cannot be annotated, this member is guarded by the relevant
  // PmmNode loaned list lock.
  [[maybe_unused]] bool used_ = false;
  // List of pages presently owned by this object. Every page in this list is defined to be in the
  // ALLOC state with |owner| set to this object.
  // Although the lock cannot be annotated, this member is guarded by the relevant
  // PmmNode loaned list lock.
  [[maybe_unused]] VmPageDoublyLinkedList pages_;

  struct Waiter : fbl::SinglyLinkedListable<Waiter*> {
    Event event;
  };
  // Maintain a list of waiters to be notified once pages_ have been freed. The Waiter object itself
  // is stack allocated in the WithLoanedPage method and registered into this list. Having this be a
  // list of Events of single waiting thread, instead of a single Event with a list of waiting
  // threads, allows the waiters to retain a reference to the FLPH object while waiting. This
  // ensures that once FinishFreeLoanedPages performs the signal on the waiters, the FLPH object can
  // be safely destroyed.
  [[maybe_unused]] fbl::SinglyLinkedList<Waiter*> waiters_;
  friend PmmNode;
};

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_NODE_H_
