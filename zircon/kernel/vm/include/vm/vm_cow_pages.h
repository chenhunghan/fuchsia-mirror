// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_VM_COW_PAGES_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_VM_COW_PAGES_H_

#include <assert.h>
#include <lib/fit/result.h>
#include <lib/page/size.h>
#include <lib/user_copy/user_ptr.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <stdint.h>
#include <zircon/types.h>

#include <fbl/array.h>
#include <fbl/canary.h>
#include <fbl/enum_bits.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/macros.h>
#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <kernel/mutex.h>
#include <ktl/utility.h>
#include <vm/compressor.h>
#include <vm/continuous_attribution_tracker.h>
#include <vm/page_source.h>
#include <vm/physical_page_borrowing_config.h>
#include <vm/pmm.h>
#include <vm/pmm_node.h>
#include <vm/stream_size_manager.h>
#include <vm/vm.h>
#include <vm/vm_aspace.h>
#include <vm/vm_constants.h>
#include <vm/vm_object.h>
#include <vm/vm_page_list.h>

// Forward declare these so VmCowPages helpers can accept references.
class BatchPQRemove;
class VmObjectPaged;
class DiscardableVmoTracker;
class PriorityChanger;

enum class VmCowPagesOptions : uint32_t {
  // Externally-usable flags:
  kNone = 0u,
  kUserPagerBackedRoot = (1u << 0),
  kPageSourceRoot = (1u << 1),

  // With this clear, zeroing a page tries to decommit the page.  With this set, zeroing never
  // decommits the page.  Currently this is only set for contiguous VMOs.
  //
  // TODO(dustingreen): Once we're happy with the reliability of page borrowing, we should be able
  // to relax this restriction.  We may still need to flush zeroes to RAM during reclaim to mitigate
  // a hypothetical client incorrectly assuming that cache-clean status will remain intact while
  // pages aren't pinned, but that mitigation should be sufficient (even assuming such a client) to
  // allow implicit decommit when zeroing or when zero scanning, as long as no clients are doing DMA
  // to/from contiguous while not pinned.
  kCannotDecommitZeroPages = (1u << 2),

  // Internal-only flags:
  kHidden = (1u << 3),

  kInternalOnlyMask = kHidden,
};
FBL_ENABLE_ENUM_BITS(VmCowPagesOptions)

// The Rust `VmCowRange` in vm_cow_pages.rs is a `#[repr(C)]` layout-compatible mirror
// of this struct. Do not change its size or layout without updating both sides.
struct VmCowRange {
  uint64_t offset;
  uint64_t len;

  constexpr VmCowRange() : offset(0), len(0) {}
  constexpr VmCowRange(uint64_t offset, uint64_t len) : offset(offset), len(len) {}

  uint64_t end() const { return offset + len; }
  bool is_empty() const { return len == 0; }
  bool is_page_aligned() const { return IsPageRounded(offset) && IsPageRounded(len); }
  VmCowRange ExpandTillPageAligned() const {
    const uint64_t start = RoundDownPageSize(offset);
    return VmCowRange(start, RoundUpPageSize(end()) - start);
  }

  VmCowRange TrimmedFromStart(uint64_t amount) const {
    return VmCowRange(offset + amount, len - amount);
  }
  // Returns the minimal range that covers both |this| and |other|. If these ranges are disjoint
  // then the returned range will be larger than combined length of |this| and |other| in order to
  // span both using a single range.
  VmCowRange Cover(VmCowRange other) const {
    if (is_empty()) {
      return other;
    }
    if (other.is_empty()) {
      return *this;
    }
    const uint64_t start = ktl::min(offset, other.offset);
    const uint64_t end = ktl::max(offset + len, other.offset + other.len);
    return VmCowRange(start, end - start);
  }
  VmCowRange WithLength(uint64_t new_length) const { return VmCowRange(offset, new_length); }
  bool IsBoundedBy(uint64_t max) const;
};

// Result of attempting to reclaim a single page. See VmCowPages::ReclaimPage for more details.
struct VmCowReclaimSuccess {
  enum class Type : uint8_t {
    Evict,
    Discard,
    Compress,
  } type;
  uint64_t num_pages = 0;
  uint64_t num_loaned_pages = 0;
};
enum class VmCowReclaimFailure : uint8_t {
  EvictAccessed,
  CompressAccessed,
  CompressFailed,
  IncorrectPage,
  Other,
};
using VmCowReclaimResult = fit::result<VmCowReclaimFailure, VmCowReclaimSuccess>;

enum class PageSourceType : uint8_t {
  // The VmCowPages is not directly backed by a page_source().
  Anonymous = 0,
  // The VmCowPages is directly backed by a contiguous page source. That has the following
  // implications:
  // * The direct page source provides specific physical pages.
  // * The direct page source (possibly with the help of the VmCowPages) provides zeroed pages.
  Contiguous,
  // The VmCowPages is directly backed by a user pager. That has the following implications:
  // * The direct page source is responsible for freeing the page content.
  // * The VmCowPages must implement dirty tracking.
  UserPager,
};

#if EXPERIMENTAL_CONTINUOUS_PER_VMO_ATTRIBUTION_ENABLED
using AttributionTracker = ContinuousAttributionTracker;
#else   // EXPERIMENTAL_CONTINUOUS_PER_VMO_ATTRIBUTION_ENABLED
using AttributionTracker = StubContinuousAttributionTracker;
#endif  // EXPERIMENTAL_CONTINUOUS_PER_VMO_ATTRIBUTION_ENABLED

class ScopedPageFreedList;

extern "C" void cpp_vm_cow_pages_free(VmCowPages* cow);
extern "C" void* cpp_vm_cow_pages_state(VmCowPages* cow);
extern "C" __PURE DiscardableVmoTracker* cpp_vm_cow_pages_discardable_tracker(
    const VmCowPages* cow);
extern "C" void cpp_vm_cow_pages_range_change_update_cow_children(VmCowPages* cow, uint64_t offset,
                                                                  uint64_t len,
                                                                  VmObject::RangeChangeOp op);
extern "C" void cpp_vm_cow_pages_resize_update_child_limits(VmCowPages* cow, uint64_t s);
// |deferred| is a VmCowPages::DeferredOps*, which cannot be named before VmCowPages is defined.
extern "C" zx_status_t cpp_vm_cow_pages_replace_page_locked(VmCowPages* cow, vm_page_t* before_page,
                                                            uint64_t offset, bool with_loaned,
                                                            vm_page_t** after_page, void* deferred,
                                                            AnonymousPageRequest* page_request);
extern "C" void rust_scoped_page_freed_list_init(ScopedPageFreedList* list);
extern "C" void rust_scoped_page_freed_list_destroy(ScopedPageFreedList* list);
extern "C" void rust_scoped_page_freed_list_free_pages(ScopedPageFreedList* list,
                                                       const VmCowPages* cow_pages);
extern "C" VmPageDoublyLinkedList* rust_scoped_page_freed_list_list(ScopedPageFreedList* list);
extern "C" FreeLoanedPagesHolder* rust_scoped_page_freed_list_flph(ScopedPageFreedList* list);

// Implements a copy-on-write hierarchy of pages in a VmPageList.
// VmCowPages have a life cycle where they start in an Init state to allow them to have
// initialization finished outside the constructor. A VmCowPages in the Init state may be
// destructed, although it is not allowed to have any pages put in it.
// Once transitioned to the Alive state the VmCowPages may generally be used, and must be
// explicitly transitioned to the Dead state prior to being destructed. The explicit transition
// ensures that a VmCowPages does not own any pages whilst in its destructor, and hence while the
// object is unreachable due to having a ref count of 0.
class VmCowPages final : public fbl::ContainableBaseClasses<
                             fbl::TaggedDoublyLinkedListable<VmCowPages*, internal::ChildListTag>>,
                         public fbl::RefCountedUpgradeable<VmCowPages> {
 public:
  static zx_status_t Create(VmCowPagesOptions options, uint32_t pmm_alloc_flags, uint64_t size,
                            ktl::unique_ptr<DiscardableVmoTracker> discardable_tracker,
                            fbl::RefPtr<VmCowPages>* cow_pages);

  static zx_status_t CreateExternal(fbl::RefPtr<PageSource> src, VmCowPagesOptions options,
                                    uint64_t size, fbl::RefPtr<VmCowPages>* cow_pages);

  // Define the lock retrieval functions differently depending on whether we should be returning a
  // local lock instance, or the common one in the hierarchy_state_ptr. Due to the TA_RET_CAP
  // statements we cannot perform |if constexpr| or equivalent indirection in the function body, and
  // must have two completely different function definitions.
  // In the absence of a local lock it is assumed, and enforced in vm_object_lock.h, that there is a
  // shared lock in the hierarchy state. If there is both a local and a shared lock then the local
  // lock is to be used for the improved lock tracking.
  Lock<CriticalMutex>* lock() const TA_RET_CAP(lock_) { return &lock_; }
  Lock<CriticalMutex>& lock_ref() const TA_RET_CAP(lock_) { return lock_; }

  uint64_t lock_order() const;

  // Similar to LockedPtr, but holds a RefPtr instead of a raw pointer.
  class LockedRefPtr {
   public:
    LockedRefPtr() = default;
    ~LockedRefPtr() { release(); }
    LockedRefPtr(LockedRefPtr&& l) = default;
    explicit LockedRefPtr(const fbl::RefPtr<VmCowPages>& object)
        : LockedRefPtr(object, object->lock_order()) {}
    LockedRefPtr(fbl::RefPtr<VmCowPages> object, uint64_t lock_order)
        : ptr_(ktl::move(object)),
          lock_(Guard<CriticalMutex>(AssertOrderedLock, ptr_->lock(), lock_order).take()) {}
    ktl::pair<fbl::RefPtr<VmCowPages>, Guard<CriticalMutex>::Adoptable> take() {
      return {ktl::move(ptr_), ktl::move(lock_)};
    }

    VmCowPages& locked() const TA_ASSERT(locked().lock()) { return *ptr_; }

    fbl::RefPtr<VmCowPages>&& release() {
      if (ptr_) {
        Guard<CriticalMutex> guard{AdoptLock, ptr_->lock(), ktl::move(lock_)};
      }
      return ktl::move(ptr_);
    }

    VmCowPages* get() const { return ptr_.get(); }
    VmCowPages& operator*() const { return *ptr_; }
    VmCowPages* operator->() const { return get(); }

    explicit operator bool() const { return !!ptr_; }
    LockedRefPtr& operator=(LockedRefPtr&& other) {
      release();
      ptr_ = ktl::move(other.ptr_);
      lock_ = ktl::move(other.lock_);
      return *this;
    }

   private:
    fbl::RefPtr<VmCowPages> ptr_;
    Guard<CriticalMutex>::Adoptable lock_;
  };
  class DeferredOps;

  // Creates a copy-on-write clone with the desired parameters. This can fail due to various
  // internal states not being correct.
  zx::result<LockedRefPtr> CreateCloneLocked(SnapshotType type, bool require_unidirectional,
                                             VmCowRange range, DeferredOps& ops) TA_REQ(lock());

  // VmCowPages are initially created in the Init state and need to be transitioned to Alive prior
  // to being used. This is exposed for VmObjectPaged to call after ensuring that creation is
  // successful, i.e. after it can guarantee that it will transition this cow pages to Dead prior to
  // it being destroyed.
  void TransitionToAliveLocked() TA_REQ(lock());

  // Returns the size in bytes of this cow pages range. This will always be a multiple of the page
  // size.
  uint64_t size_locked() const TA_REQ(lock());

  // TODO(https://github.com/rust-lang/rust/issues/163911): The options()-derived predicates below
  // (through node_has_parent_content_markers()) are deliberately C++ inlines over options() rather
  // than forwarders to Rust: rustc currently omits `zeroext` on `extern "C"` bool returns on
  // x86-64, so a bit-0 flag test can reach C++ with bits 1..7 of %al set and read as true.

  // Returns whether this cow pages node is ultimately backed by a user pager to fulfill initial
  // content, and not zero pages.  Contiguous VMOs have page_source_ set, but are not pager backed
  // in this sense.
  //
  // This should only be used to report to user mode whether a VMO is user-pager backed, not for any
  // other purpose.
  bool is_root_source_user_pager_backed() const {
    return !!(options() & VmCowPagesOptions::kUserPagerBackedRoot);
  }

  // Returns whether the root of the cow pages hierarchy has non-null page_source_.
  bool root_has_page_source() const { return !!(options() & VmCowPagesOptions::kPageSourceRoot); }

  // Helper function for CowPage cloning methods. Returns any options that should be passed down to
  // the child.
  VmCowPagesOptions inheritable_options() const {
    return VmCowPagesOptions::kNone | (options() & (VmCowPagesOptions::kUserPagerBackedRoot |
                                                    VmCowPagesOptions::kPageSourceRoot));
  }

  bool is_root_source_preserving_page_content() const {
    return !!(options() & VmCowPagesOptions::kUserPagerBackedRoot);
  }

  bool is_parent_hidden_locked() const TA_REQ(lock());

  bool has_no_children_locked() const TA_REQ(lock());

  bool is_discardable() const { return !!discardable_tracker_; }

  bool can_evict() const;

  bool can_root_source_evict() const {
    bool result = is_root_source_preserving_page_content();
    DEBUG_ASSERT(result == is_root_source_user_pager_backed());
    return result;
  }

  // can_borrow() returns true if the VmCowPages is capable of borrowing pages, but whether
  // the VmCowPages should actually borrow pages also depends on a borrowing-site-specific flag that
  // the caller is responsible for checking (in addition to checking can_borrow()).  Only if
  // both are true should the caller actually borrow at the caller's specific potential borrowing
  // site.
  // Aside from the general borrowing in the PhysicalPageBorrowingConfig being turned on and
  // off, the ability to borrow is constant over the lifetime of the VmCowPages.
  bool can_borrow() const;

  // In addition to whether a VmCowPages is allowed, for correctness reasons, to borrow pages there
  // are other, potentially variable, factors that influence whether it's considered a good idea for
  // this VmCowPages to borrow pages. In particular it's possible for this to change over the
  // lifetime of the VmCowPages.
  bool should_borrow_locked() const TA_REQ(lock()) {
    if (!can_borrow()) {
      return false;
    }
    // Exclude is_latency_sensitive_ to avoid adding latency due to reclaim.
    //
    // Currently we evict instead of replacing a page when reclaiming, so we want to avoid evicting
    // pages that are latency sensitive or are fairly likely to be pinned at some point.
    //
    // We also don't want to borrow a page that might get pinned again since we want to mitigate the
    // possibility of an invalid DMA-after-free.
    const bool excluded_from_borrowing_for_latency_reasons =
        high_priority_count_locked() != 0 || ever_pinned_locked();
    return !excluded_from_borrowing_for_latency_reasons;
  }

  // Returns whether this cow pages node is dirty tracked.
  bool is_dirty_tracked() const;

  // If true this node, and all nodes in this hierarchy, are using parent content markers to
  // indicate when a leaf node may need to walk up the tree to find content.
  //
  // When parent content markers are in use an empty page list slot in a leaf node means that there
  // is *no* visible parent content above, and the parent hierarchy does not have to be searched.
  //
  // For memory efficiency, and because it would be redundant, parent content markers are never
  // placed in the hidden nodes, only leaf nodes.
  //
  // The presence of a parent content marker in a leaf node indicates that there *might* be content
  // in a parent node and that a tree walk *must* be performed to search for it. The reason for
  // spurious parent content markers is that zero page deduplication could happen on hidden nodes,
  // which could remove the content, but leave the parent content markers in the leaf nodes.
  // Spurious parent content markers can persist in VMOs when they lose their parent. Spurious
  // parent content markers are redundant and could be cleaned up.
  //
  // Use of parent content markers is just the inverse of having a page source, since if there is a
  // page source we always have to go to it for content as the zero page cannot be assumed. Although
  // some page sources do supply zero content (physical page provider for contiguous VMOs),
  // optimizing this check for that is redundant since such page sources do not support
  // copy-on-write, and so never have children to begin with.
  bool tree_has_parent_content_markers() const { return !root_has_page_source(); }

  // Indicates whether this node can have parent content markers placed in it. This is just checking
  // if it is both a leaf node, and the tree overall can have parent content markers.
  //
  // Note that even if this is false, if |tree_has_parent_content_markers| is true then reasoning
  // may need to be done about parent content markers.
  bool node_has_parent_content_markers() const {
    return !is_hidden() && tree_has_parent_content_markers();
  }

  // The modified state is only supported for root pager-backed VMOs, and will get queried (and
  // possibly reset) on the next QueryPagerVmoStatsLocked() call. Although the modified state is
  // only tracked for the root VMO.
  void mark_modified_locked() TA_REQ(lock());

  bool is_high_memory_priority_locked() const TA_REQ(lock());

  // See description on |pinned_page_count_| for meaning.
  uint64_t pinned_page_count_locked() const TA_REQ(lock());

  // Sets the VmObjectPaged backlink for this copy-on-write node.
  // Currently it is assumed that all nodes always have backlinks with the 1:1 hierarchy mapping,
  // unless this is a hidden node.
  void set_paged_backlink_locked(VmObjectPaged* ref) TA_REQ(lock()) { paged_ref_ = ref; }

  VmObjectPaged* get_paged_backlink_locked() const TA_REQ(lock()) { return paged_ref_; }

  uint64_t HeapAllocationBytesLocked() const TA_REQ(lock());

  uint64_t ReclamationEventCountLocked() const TA_REQ(lock());

  void DetachSource();

  ktl::optional<zx_koid_t> GetPageSourceKoid() const;

  // Resizes the range of this cow pages. |size| must be a multiple of the page size.
  zx_status_t Resize(uint64_t size) TA_EXCL(lock());

  // See VmObject::Lookup
  zx_status_t LookupLocked(VmCowRange range, VmObject::LookupFunction lookup_fn) TA_REQ(lock());

  // Similar to LookupLocked, but enumerate all readable pages in the hierarchy within the requested
  // range. The offset passed to the |lookup_fn| is the offset this page is visible at in this
  // object, even if the page itself is committed in a parent object. The physical addresses given
  // to the lookup_fn should not be retained in any way unless the range has also been pinned by the
  // caller.
  // Ranges of length zero are considered invalid and will return ZX_ERR_INVALID_ARGS. The lookup_fn
  // can terminate iteration early by returning ZX_ERR_STOP.
  using LookupReadableFunction = VmObject::LookupFunction;
  zx_status_t LookupReadableLocked(VmCowRange range, LookupReadableFunction lookup_fn)
      TA_REQ(lock());

  // See VmObject::TakePages
  //
  // May return ZX_ERR_SHOULD_WAIT if the |page_request| is filled out and needs waiting on. In this
  // case |taken_len| might be populated with a value less than |len|.
  //
  // |taken_len| is always filled with the amount of |len| that has been processed to allow for
  // gradual progress of calls. Will always be equal to |len| if ZX_OK is returned. Similarly the
  // |splice_offset| indicates the base offset in |pages| where the content should be inserted.
  zx_status_t TakePages(VmCowRange range, uint64_t splice_offset, VmPageSpliceList* pages,
                        uint64_t* taken_len, MultiPageRequest* page_request);

  // Pre-process a VmPageSpliceList for supply.
  //
  // If there is a page source, references are converted to pages. When allocating pages from
  // references, ZX_ERR_SHOULD_WAIT can be returned which is managed by this function.
  // TODO(sagebarred): handle InitializeVmPage from PhysicalPageProvider here.
  zx_status_t ProcessPagesForSupply(VmPageSpliceList* pages);

  // See VmObject::SupplyPages
  //
  // Should never return ZX_ERR_SHOULD_WAIT, waiting on page requests is managed by
  // ProcessPagesForSupply.
  zx_status_t SupplyPagesLocked(VmCowRange range, VmPageSpliceList* pages, SupplyOptions options,
                                DeferredOps& deferred, MultiPageRequest* page_request)
      TA_REQ(lock());

  // See VmObject::FailPageRequests
  zx_status_t FailPageRequestsLocked(VmCowRange range, zx_status_t error_status) TA_REQ(lock());

  // Used to track dirty_state in the vm_page_t.
  //
  // The transitions between the three states can roughly be summarized as follows:
  // 1. A page starts off as Clean when supplied.
  // 2. A write transitions the page from Clean to Dirty.
  // 3. A writeback_begin moves the Dirty page to AwaitingClean.
  // 4. A writeback_end moves the AwaitingClean page to Clean.
  // 5. A write that comes in while the writeback is in progress (i.e. the page is AwaitingClean)
  // moves the AwaitingClean page back to Dirty.
  enum class DirtyState : uint8_t {
    // The page does not track dirty state. Used for non pager backed pages.
    Untracked = 0,
    // The page is clean, i.e. its contents have not been altered from when the page was supplied.
    Clean,
    // The page's contents have been modified from the time of supply, and should be written back to
    // the page source at some point.
    Dirty,
    // The page still has modified contents, but the page source is in the process of writing back
    // the changes. This is used to ensure that a consistent version is written back, and that any
    // new modifications that happen during the writeback are not lost. The page source will mark
    // pages AwaitingClean before starting any writeback.
    AwaitingClean,
    NumStates,
  };
  // Make sure that the state can be encoded in the vm_page_t's dirty_state field.
  static_assert(static_cast<uint8_t>(DirtyState::NumStates) <= VM_PAGE_OBJECT_MAX_DIRTY_STATES);

  static bool is_page_dirty_tracked(const vm_page_t* page) {
    return DirtyState(page->object.dirty_state) != DirtyState::Untracked;
  }
  static bool is_page_dirty(const vm_page_t* page) {
    return DirtyState(page->object.dirty_state) == DirtyState::Dirty;
  }
  static bool is_page_clean(const vm_page_t* page) {
    return DirtyState(page->object.dirty_state) == DirtyState::Clean;
  }
  static bool is_page_reclaimable(const vm_page_t* page) {
    return PageQueues::IsPageReclaimable(page);
  }

  // See VmObject::DirtyPages. |page_request| is required to support delayed PMM allocations; if
  // ZX_ERR_SHOULD_WAIT is returned the caller should wait on |page_request|. |alloc_list| will hold
  // any pages that were allocated but not used in case of delayed PMM allocations, so that it can
  // be reused across multiple successive calls whilst ensuring forward progress.
  zx_status_t DirtyPages(VmCowRange range, VmPageDoublyLinkedList* alloc_list,
                         AnonymousPageRequest* page_request);

  using DirtyRangeEnumerateFunction = VmObject::DirtyRangeEnumerateFunction;
  // See VmObject::EnumerateDirtyRanges
  zx_status_t EnumerateDirtyRangesLocked(VmCowRange range,
                                         DirtyRangeEnumerateFunction&& dirty_range_fn)
      TA_REQ(lock());

  // Query pager VMO |stats|, and reset them too if |reset| is set to true.
  zx_status_t QueryPagerVmoStatsLocked(bool reset, zx_pager_vmo_stats_t* stats) TA_REQ(lock());

  void ResetPagerVmoStatsLocked() TA_REQ(lock());

  // See VmObject::WritebackBegin
  zx_status_t WritebackBeginLocked(VmCowRange range, bool is_zero_range) TA_REQ(lock());

  // See VmObject::WritebackEnd
  zx_status_t WritebackEndLocked(VmCowRange range) TA_REQ(lock());

  // Tries to prepare the range [offset, offset + len) for writing by marking pages dirty or
  // verifying that they are already dirty. It is possible for only some or none of the pages in the
  // range to be dirtied at the end of this call. |dirty_len_out| will return the (page-aligned)
  // length starting at |offset| that contains dirty pages, either already dirty before making the
  // call or dirtied during the call. In other words, the range [offset, offset + dirty_len_out)
  // will be dirty when this call returns, i.e. prepared for the write to proceed, where
  // |dirty_len_out| <= |len|.
  //
  // If the specified range starts with pages that are not already dirty and need to request the
  // page source before transitioning to dirty, a DIRTY page request will be forwarded to the page
  // source. In this case |dirty_len_out| will be set to 0, ZX_ERR_SHOULD_WAIT will be returned and
  // the caller should wait on |page_request|. If no page requests need to be generated, i.e. we
  // could find some pages that are already dirty at the start of the range, or if the VMO does not
  // require dirty transitions to be trapped, ZX_OK is returned.
  //
  // |offset| and |len| should be page-aligned.
  zx_status_t PrepareForWriteLocked(VmCowRange range, LazyPageRequest* page_request,
                                    uint64_t* dirty_len_out) TA_REQ(lock());

  class LookupCursor;
  // See VmObjectPaged::GetLookupCursorLocked
  zx::result<LookupCursor> GetLookupCursorLocked(VmCowRange range) TA_REQ(lock());

  // Controls the type of VmPageOrMarker slot in |this| VmCowPages' page_list_ that can be
  // overwritten by the Add[New]Page[s]Locked functions. It is the caller's responsibility to ensure
  // that the previous content is dealt with correctly (e.g. any pages and compressed
  // references are freed).
  enum class CanOverwriteSlot : uint8_t {
    // Only overwrite empty slots.
    Empty,
    // Can overwrite parent content markers (in addition to empty slots).
    EmptyOrParent,
    // Can overwrite zero page markers and zero intervals (in addition to empty slots and
    // parent content markers).
    ZeroMarkerOrInterval,
    // Can overwrite pages and compressed references (in addition to empty slots, parent content
    // markers, zero markers and intervals), i.e. can overwrite all VmPageOrMarker types.
    PageOrRef,
  };
  // Adds an allocated page to this cow pages at the specified offset, can be optionally zeroed and
  // any mappings invalidated. If an error is returned the caller retains ownership of |page|.
  // Offset must be page aligned. Mappings being invalidated is controlled by |deferred| where if
  // a nullptr is passed then no mappings, in this object or any child, will be invalidated. If
  // |deferred| is non-null then mappings will be invalidated if necessary, both immediately in this
  // object, and via |deferred| for the children.
  //
  // |overwrite| controls how the function handles pre-existing content at |offset|. If |overwrite|
  // does not permit replacing the content, ZX_ERR_ALREADY_EXISTS will be returned. If a page is
  // released from the page list as a result of overwriting, it is returned through |released_page|
  // and the caller takes ownership of this page. If the |overwrite| action is such that a page
  // cannot be released, it is valid for the caller to pass in nullptr for |released_page|.
  zx_status_t AddNewPageLocked(uint64_t offset, vm_page_t* page, CanOverwriteSlot overwrite,
                               VmPageOrMarker* released_page, bool zero, DeferredOps* deferred)
      TA_REQ(lock());

  // Adds a set of pages consecutively starting from the given offset. Regardless of the return
  // result ownership of the pages is taken. Pages are assumed to be in the ALLOC state and can be
  // optionally zeroed before inserting. start_offset must be page aligned.
  //
  // |overwrite| controls how the function handles pre-existing content in the range, however it is
  // not valid to specify the |CanOverwriteSlot::PageOrRef| option, as any pages or compressed
  // references that would get released as a consequence cannot be returned.
  zx_status_t AddNewPagesLocked(uint64_t start_offset, VmPageDoublyLinkedList* pages,
                                CanOverwriteSlot overwrite, bool zero, DeferredOps* deferred)
      TA_REQ(lock());

  // Attempts to release pages in the pages list causing the range to become copy-on-write again.
  // For consistency if there is a parent or a backing page source, such that the range would not
  // explicitly copy-on-write the zero page then this will fail. Use ZeroPagesLocked for an
  // operation that is guaranteed to succeed, but may not release memory.
  zx_status_t DecommitRange(VmCowRange range) TA_EXCL(lock());

  // After successful completion the range of pages will all read as zeros. The mechanism used to
  // achieve this is not guaranteed to decommit, but it will try to.
  // |range| must be page aligned offsets within the range of the object. |dirty_track| specifies
  // whether the range being zeroed subscribes to dirty tracking, if |true| the range will start out
  // as dirty. |dirty_track| only has meaning if the VMO supports dirty tracking, otherwise it is
  // ignored.
  //
  // Returns a status code, and the number of bytes that were actually zeroed.
  // This may be nonzero even if the returned status != ZX_OK.
  //
  // The returned status is one of the following:
  //  * ZX_OK => The whole range was successfully zeroed.
  //  * ZX_ERR_SHOULD_WAIT => The caller needs to wait on the |page_request| and then retry the
  //    operation. The caller may advance the start offset by the number of zeroed bytes before
  //    retrying.
  //  * Any other error code indicates a failure to zero a part of the range or the whole
  //  range.
  ktl::pair<zx_status_t, uint64_t> ZeroPagesLocked(VmCowRange range, bool dirty_track,
                                                   DeferredOps& deferred,
                                                   MultiPageRequest& page_request) TA_REQ(lock());

  // Attempts to commit a range of pages. This has three kinds of return status
  //  ZX_OK => The whole range was successfully committed and |len| will be written to
  //           |committed_len|
  //  ZX_ERR_SHOULD_WAIT => A partial (potentially 0) range was committed (output in |committed_len|
  //                        and the passed in |page_request| should be waited on before retrying
  //                        the commit operation. The portion that was successfully committed does
  //                        not need to retried.
  //  * => Any other error, the number of pages committed is undefined.
  // The |offset| and |len| are assumed to be page aligned and within the range of |size_|.
  zx_status_t CommitRangeLocked(VmCowRange range, DeferredOps& deferred, uint64_t* committed_len,
                                MultiPageRequest& page_request) TA_REQ(lock());

  // Increases the pin count of the range of pages given by |offset| and |len|. The full range must
  // already be committed and this either pins all pages in the range, or pins no pages and returns
  // an error. The caller can assume that on success len / kPageSize pages were pinned.
  // The |offset| and |len| are assumed to be page aligned and within the range of |size_|.
  // All pages in the specified range are assumed to be non-loaned pages, so the caller is expected
  // to replace any loaned pages beforehand if required.
  zx_status_t PinRangeLocked(VmCowRange range) TA_REQ(lock());

  // See VmObject::Unpin
  // An optional |DeferredOps| can be provided for the purposes of performing extra debugging
  // checks, but otherwise has no functional requirement. The debug checks are optional as call
  // sites may not be able to satisfy the locking requirements to construct a DeferredOps, and may
  // know (due to be undoing a pin they themselves had started), that no checks need to be done.
  void UnpinLocked(VmCowRange range, DeferredOps* deferred) TA_REQ(lock());

  // See VmObject::DebugIsRangePinned
  bool DebugIsRangePinnedLocked(VmCowRange range) TA_REQ(lock());

  // Returns true if a page is not currently committed, and if the offset were to be read from, it
  // would be read as zero. Requested offset must be page aligned and within range.
  bool PageWouldReadZeroLocked(uint64_t page_offset) TA_REQ(lock());

  // see VmObject::GetAttributedMemoryInRange
  using AttributionCounts = VmObject::AttributionCounts;
  AttributionCounts GetAttributedMemoryInRangeLocked(VmCowRange range) const TA_REQ(lock());

  enum class EvictionAction : uint8_t {
    FollowHint,
    IgnoreHint,
  };

  // Asks the VMO to attempt to reclaim the specified page. There are a few possible outcomes:
  // 1. Exactly this page is reclaimed.
  // 2. This page and other pages are reclaimed.
  // 3. Just other pages are reclaimed.
  // 4. No pages are reclaimed.
  // Pages other than the one requested may get reclaimed due to any internal relationships between
  // pages that make it meaningless or difficult to reclaim just the single page in question.
  // In the cases of (3) and (4) there are some guarantees provided:
  // 1. If the |page| was not from this VMO (or not at the specified offset) then nothing about the
  //    |page| or this VMO will be modified.
  // 2. If the |page| is from this VMO and offset (and was not reclaimed) then the page will have
  //    been removed from any candidate reclamation lists (such as the DontNeed pager backed list).
  // The effect of (2) is that the caller can assume in the case of reclamation failure it will not
  // keep finding this page as a reclamation candidate and infinitely retry it.
  // If the |compressor| is non-null then it must have just had |Arm| called on it.
  // |eviction_action| hints indicates whether the |always_need| eviction hint should be respected
  // or ignored. Require will force eviction.
  //
  // The actual number of pages reclaimed is returned if successful, or a failure reason if not.
  // VmCowReclaimResult is an alias for fit::result<VmCowReclaimFailure, VmCowReclaimSuccess>.
  VmCowReclaimResult ReclaimPage(vm_page_t* page, uint64_t offset, EvictionAction eviction_action,
                                 VmCompressor* compressor);

  // Helper for reclamation functions to perform common checks for whether or not reclamation should
  // proceed. It takes two parameters, one being the original requested page and the other being
  // the result of a page list Lookup or LookupMutable, allowing it to check if the page is still
  // up to date and owned by this VMO.
  template <typename T>
  ktl::optional<VmCowReclaimFailure> CannotReclaimPageLocked(vm_page_t* page, T actual)
      TA_REQ(lock());

  // If any pages in the specified range are loaned pages, replaces them with non-loaned pages
  // (which requires providing a |page_request|). The specified range should be fully committed
  // before calling this function. If a gap or a marker is encountered, or a loaned page cannot be
  // replaced, returns early with ZX_ERR_BAD_STATE. If the replacement needs to wait on the PMM for
  // allocation, returns ZX_ERR_SHOULD_WAIT, and the caller should wait on the |page_request|.
  // |non_loaned_len| is set to the length (starting at |offset|) that contains only non-loaned
  // pages. |offset| and |len| must be page-aligned.
  zx_status_t ReplacePagesWithNonLoanedLocked(VmCowRange range, DeferredOps& deferred,
                                              AnonymousPageRequest* page_request,
                                              uint64_t* non_loaned_len) TA_REQ(lock());

  // If page is still at offset, replace it with a loaned page.
  zx_status_t ReplacePageWithLoaned(vm_page_t* before_page, uint64_t offset) TA_EXCL(lock());

  // Attempts to dedup the given page at the specified offset with the zero page. The only
  // correctness requirement for this is that `page` must be *some* valid vm_page_t, meaning that
  // all race conditions are handled internally. This function returns false if
  //  * page is either not from this VMO, or not found at the specified offset
  //  * page is pinned
  //  * vmo is uncached
  //  * page is not all zeroes
  // Otherwise 'true' is returned and the page will have been returned to the pmm with a zero page
  // marker put in its place.
  bool DedupZeroPage(vm_page_t* page, uint64_t offset);

  void DumpLocked(uint depth, bool verbose) const TA_REQ(lock());

  // see VmObject::DebugLookupDepth
  uint32_t DebugLookupDepthLocked() const TA_REQ(lock());

  // Slow validation of the continuously tracked populated slots count.
  //
  // Returns true if the tracked populated slots count equals the number of pages, references, and
  // parent content markers in the local page list.
  //
  // Panics if the populated slots feature is not enabled.
  bool DebugValidateContinuousAttribution() const TA_REQ(lock());

  // Walks all the descendants in a preorder traversal. Stops if func returns anything other than
  // ZX_OK.
  zx_status_t DebugForEachDescendant(
      fit::inline_function<zx_status_t(VmCowPages* cow, uint depth)>);

  using RangeChangeOp = VmObject::RangeChangeOp;
  // Applies the specific operation to all mappings in the given range. The mappings for the current
  // object are operated on immediately, with any children being operated on using |deferred|. If
  // the caller knows that no |DeferredOps| is needed (e.g. the VMO has no children and is not pager
  // backed) then a nullptr can be provided.
  void RangeChangeUpdateLocked(VmCowRange range, RangeChangeOp op, DeferredOps* deferred)
      TA_REQ(lock());

  // Apply the specified operation to all mappings of |paged| in the given range. The provided
  // |range| must be page aligned.
  //
  // |paged| is assumed to be one of the aliases for this |VmCowPages| node. |range| must be
  // page-aligned.
  //
  // Differs from |RangeChangeUpdateLocked| in that this method does not apply the operation to
  // copy-on-write children.
  void RangeChangeUpdateMappingsLocked(VmObjectPaged& paged, VmCowRange range, RangeChangeOp op)
      TA_REQ(lock());

  // The VmObjectPaged is changing its cache mapping policy. Clean / invalidate all existing pages
  // and update page queues if required.
  void FinishCachePolicyTransitionLocked() TA_REQ(lock());

  // Promote pages in the specified range for reclamation under memory pressure. |offset| will be
  // rounded down to the page boundary, and |len| will be rounded up to the page boundary.
  // Currently used only for pager-backed VMOs to move their pages to the end of the
  // pager-backed queue, so that they can be evicted first.
  zx_status_t PromoteRangeForReclamation(VmCowRange range);

  // Protect pages in the specified range from reclamation under memory pressure. |offset| will be
  // rounded down to the page boundary, and |len| will be rounded up to the page boundary. Any
  // absent pages in the range will first be committed and then, if |set_always_need| is true, the
  // |always_need| flag in the pages will be set.
  // If the |ignore_errors| flag is set then any per page errors will be ignored and future pages in
  // the range will still be operated on. If this flag is not set then any kind of error causes an
  // immediate abort.
  zx_status_t ProtectRangeFromReclamation(VmCowRange range, bool set_always_need,
                                          bool ignore_errors);

  // Ensures any pages in the specified range are not compressed, but does not otherwise commit any
  // pages.
  zx_status_t DecompressInRange(VmCowRange range);

  zx_status_t LockRangeLocked(VmCowRange range, zx_vmo_lock_state_t* lock_state_out) TA_REQ(lock());
  zx_status_t TryLockRangeLocked(VmCowRange range) TA_REQ(lock());
  zx_status_t UnlockRangeLocked(VmCowRange range) TA_REQ(lock());

  uint64_t DebugGetPageCountLocked() const TA_REQ(lock());
  bool DebugIsPage(uint64_t offset) const;
  bool DebugIsMarker(uint64_t offset) const;
  uint32_t DebugGetMarkerShareCount(uint64_t offset) const;
  bool DebugIsParentContent(uint64_t offset) const;
  bool DebugIsEmpty(uint64_t offset) const;
  vm_page_t* DebugGetPage(uint64_t offset) const TA_EXCL(lock());
  vm_page_t* DebugGetPageLocked(uint64_t offset) const TA_REQ(lock());

  // Exposed for testing.
  DiscardableVmoTracker* DebugGetDiscardableTracker() const { return discardable_tracker_.get(); }

  bool DebugIsHighMemoryPriority() const TA_EXCL(lock());

  // See DiscardableVmoTracker::DebugDiscardablePageCounts().
  struct DiscardablePageCounts {
    uint64_t locked;
    uint64_t unlocked;
  };

  // Returns the parent of this cow pages, may be null. Generally the parent should never be
  // directly accessed externally, but this exposed specifically for tests.
  fbl::RefPtr<VmCowPages> DebugGetParent() const TA_EXCL(lock());

  // Unlocked wrapper around ReplacePageLocked, exposed for the physical page provider to cancel
  // loans with.
  zx_status_t ReplacePage(vm_page_t* before_page, uint64_t offset, bool with_loaned,
                          vm_page_t** after_page, AnonymousPageRequest* page_request)
      TA_EXCL(lock());

  // In the given range, evict any pages that are in `page_queue`. This function should be used when
  // we want to evict for the purpose of freeing memory, and details on the specific page don't
  // matter.
  VmCowReclaimResult ReclaimRangeForEviction(uint64_t offset, size_t length,
                                             EvictionAction eviction_action);

  // Evict a specific loaned page for the use case of reclaiming loaned pages by the physical page
  // provider. Unlike ReclaimPage this function can assume it just needs to evict, and has no
  // requirements on updating any reclamation lists.
  zx_status_t EvictLoanedPage(vm_page_t* page, uint64_t offset);

  // Potentially transitions from Alive->Dead if the cow pages is unreachable (i.e. has no
  // paged_ref_ and no children), iteratively transitioning any newly unreachable ancestors. Used by
  // VmObjectPaged when it unlinks the paged_ref_, prior to dropping the RefPtr.
  void MaybeDeadTransition();

  // Helper to allocate a new page for the VMO, filling out the page request if necessary.
  zx_status_t AllocPage(vm_page_t** page, AnonymousPageRequest* page_request);

  // Helper to free |pages| to the PMM. This function will also try to invoke FreePages() on the
  // backing page source if it supports it. Given the allowance of freeing pages from any object in
  // the hierarchy, but the page source only being on the root, it is a requirement (and checked on
  // clone creation), that if a page source is handling free then it may not have CoW children.
  // There is also an equivalent assumption that if the page source is handling free, then the page
  // source will be supplying all the pages and this object must never allocate directly from the
  // PMM.
  //
  // Callers should avoid calling pmm_free() directly from inside VmCowPages, and instead should use
  // this helper.
  void FreePages(VmPageDoublyLinkedList* pages);

  // Helper to free |pages| to the PMM. This function will also try to invoke FreePages() on the
  // backing page source if it supports it. Given the allowance of freeing pages from any object in
  // the hierarchy, but the page source only being on the root, it is a requirement (and checked on
  // clone creation), that if a page source is handling free then it may not have CoW children.
  // There is also an equivalent assumption that if the page source is handling free, then the page
  // source will be supplying all the pages and this object must never allocate directly from the
  // PMM.
  //
  // Callers should avoid calling pmm_free_page() directly from inside VmCowPages, and instead
  // should use this helper.
  void FreePage(vm_page_t* page);

  // Returns an appropriate PmmOptDelayReuse value for use when freeing pages from this object.
  PmmOptDelayReuse should_delay_reuse_on_free() const;

  static void DebugDumpReclaimCounters();

  // Test-only interface to get the current populated slots count.
  //
  // This method panics if the kernel is built without support for the functional continuous
  // attribution tracker (EXPERIMENTAL_CONTINUOUS_PER_VMO_ATTRIBUTION_ENABLED).
  uint32_t DebugGetPopulatedSlotsCount() const;

 private:
  // private constructor (use Create...())
  VmCowPages(VmCowPagesOptions options, uint32_t pmm_alloc_flags, uint64_t size,
             fbl::RefPtr<PageSource> page_source,
             ktl::unique_ptr<DiscardableVmoTracker> discardable_tracker, uint64_t lock_order);

  ~VmCowPages();

  friend class fbl::RefPtr<VmCowPages>;
  friend class LockedParentWalker;
  friend class PriorityChanger;
  friend void ::cpp_vm_cow_pages_free(VmCowPages*);
  friend void* ::cpp_vm_cow_pages_state(VmCowPages* cow);
  friend DiscardableVmoTracker* ::cpp_vm_cow_pages_discardable_tracker(const VmCowPages* cow);
  friend void ::cpp_vm_cow_pages_range_change_update_cow_children(VmCowPages* cow, uint64_t offset,
                                                                  uint64_t len,
                                                                  VmObject::RangeChangeOp op);
  friend void ::cpp_vm_cow_pages_resize_update_child_limits(VmCowPages* cow, uint64_t s);
  friend zx_status_t(::cpp_vm_cow_pages_replace_page_locked)(VmCowPages* cow,
                                                             vm_page_t* before_page,
                                                             uint64_t offset, bool with_loaned,
                                                             vm_page_t** after_page, void* deferred,
                                                             AnonymousPageRequest* page_request);

  DISALLOW_COPY_ASSIGN_AND_MOVE(VmCowPages);

  // Helper class for managing a locked VmCowPages referenced by a raw pointer. This helper makes it
  // easy pass around references to locked objects while retaining as much static analysis support
  // as possible.
  // This class needs to be declared fully inline here so that VmCowPages methods can reference it
  // and so that this can reference the |lock()| member of VmCowPages.
  class LockedPtr {
   public:
    LockedPtr() = default;
    ~LockedPtr() { release(); }
    LockedPtr(LockedPtr&& other) : ptr_(other.ptr_), lock_(other.take_lock()) {}
    explicit LockedPtr(VmCowPages* ptr) : LockedPtr(ptr, ptr->lock_order()) {}
    LockedPtr(VmCowPages* ptr, uint64_t lock_order) TA_EXCL(ptr->lock())
        : ptr_(ptr),
          lock_(Guard<CriticalMutex>{AssertOrderedLock, ptr->lock(), lock_order}.take()) {}
    // Take both the pointer and the lock, leaving the LockedPtr empty. Caller must take ownership
    // of the returned lock and release it.
    ktl::pair<VmCowPages*, Guard<CriticalMutex>::Adoptable> take() {
      VmCowPages* ret = ptr_;
      return {ret, take_lock()};
    }
    // Provide locked access to the underlying pointer. Must not be null.
    VmCowPages& locked() const TA_ASSERT(locked().lock()) { return *ptr_; }
    // Provide locked access toe the underlying pointer, or if the pointer is null locked access to
    // the passed in object.
    VmCowPages& locked_or(VmCowPages* self) const TA_REQ(self->lock())
        TA_ASSERT(locked_or(self).lock()) {
      if (ptr_) {
        return *ptr_;
      }
      return *self;
    }
    const VmCowPages& locked_or(const VmCowPages* self) const TA_REQ(self->lock())
        TA_ASSERT(locked_or(self).lock()) {
      if (ptr_) {
        return *ptr_;
      }
      return *self;
    }
    // Release the lock, returning the underlying pointer.
    VmCowPages* release() {
      VmCowPages* ret = ptr_;
      if (ret) {
        release_internal();
      }
      return ret;
    }

    explicit operator bool() const { return !!ptr_; }
    VmCowPages* get() const { return ptr_; }
    VmCowPages& operator*() const { return *ptr_; }
    VmCowPages* operator->() const { return ptr_; }

    LockedPtr& operator=(LockedPtr&& other) {
      release();
      auto [ptr, lock] = other.take();
      // Whatever ptr and lock are they come from a LockedPtr that is assumed to be valid, and so
      // assigning them into ourselves is assumed to valid and maintain our lock invariant.
      ptr_ = ptr;
      lock_ = ktl::move(lock);
      return *this;
    }

   private:
    // Helper for moving out the lock_ and clearing the ptr_ at the same time.
    Guard<CriticalMutex>::Adoptable&& take_lock() {
      ptr_ = nullptr;
      return ktl::move(lock_);
    }

    void release_internal() {
      Guard<CriticalMutex> guard{AdoptLock, ptr_->lock(), ktl::move(lock_)};
      ptr_ = nullptr;
    }
    // Underlying object pointer and lock. The invariant that this class maintains is that if ptr_
    // is null, then lock_ is invalid, otherwise if ptr_ is non-null then lock_ holds the adoptable
    // lock acquisition of that object.
    VmCowPages* ptr_ = nullptr;
    Guard<CriticalMutex>::Adoptable lock_;
  };

  // Helper for determining whether the current node should perform a dead transition or not.
  bool should_dead_transition_locked() const TA_REQ(lock()) {
    return !paged_ref_ && children_list_len_locked() == 0 &&
           life_cycle_locked() == LifeCycle::Alive;
  }

  // Fast path for transitioning a root VmCowPages (with no parent_) from Alive->Dead without
  // allocating parent/sibling LockedPtrs or returning a deferred parent RefPtr.
  void DeadTransitionRootLocked() TA_REQ(lock());

  // Outlined slow paths for MaybeDeadTransition when |parent_| is non-null.
  __NO_INLINE void MaybeDeadTransitionWithParent(Guard<CriticalMutex>::Adoptable adopt);
  fbl::RefPtr<VmCowPages> MaybeDeadTransitionWithParentSingle(Guard<CriticalMutex>::Adoptable adopt,
                                                              bool* out_retry);

  // Transitions a child VmCowPages (with non-null |parent_|) from Alive->Dead, freeing pages and
  // cleaning up state. Responsibility of the caller to validate that it is correct to be doing this
  // transition. |parent| must be a locked pointer to |parent_| and |sibling| must be as documented
  // in |RemoveChildLocked|. Returns its former |parent_| RefPtr, which the caller must check if a
  // dead transition is needed and then release the RefPtr.
  fbl::RefPtr<VmCowPages> DeadTransitionLocked(const LockedPtr& parent, const LockedPtr& sibling)
      TA_REQ(lock());

  // A hidden VmCowPages has no associated VMO.
  bool is_hidden() const;
  bool can_decommit_zero_pages() const;

  PageSourceType page_source_type() const;

  // Returns whether or not performing a bidirectional clone would result in a valid tree structure.
  // This does not perform checks on whether there are pinned pages, or if a bidirectional clone
  // would semantically make sense. Additionally the target |parent| for the new node should be
  // passed in, which may or may not be the same as |parent_locked()|.
  bool can_bidirectional_clone_locked(const LockedPtr& parent) const TA_REQ(lock()) {
    // If the immediate node has a page source of any kind then bidirectional cloning is not
    // possible. A page source is otherwise permitted in the tree.
    if (page_source()) {
      return false;
    }

    // Children may not exist on the current node, as the bidirectional clone path cannot presently
    // fix them up.
    if (children_list_len_locked() != 0) {
      return false;
    }

    // If there is a parent then either that parent is hidden, or the parent is the root of the
    // tree. This forbids creating a bi-directional clone at the end of chain of unidirectional
    // clones.
    if (parent && parent.locked().parent_locked() && !parent->is_hidden()) {
      return false;
    }

    return true;
  }

  // Returns whether or not performing a unidirectional clone would result in a valid tree
  // structure. This does not mean that the a unidirectional clone would semantically make sense.
  bool can_unidirectional_clone_locked() const TA_REQ(lock()) {
    // Root must be pager-backed, otherwise we must always be doing a bidirectional clone.
    if (!is_root_source_user_pager_backed()) {
      return false;
    }

    // Any parent must not be hidden. This transitively ensures that there is a never a
    // unidirectional clone anywhere below a hidden parent.
    if (is_parent_hidden_locked()) {
      return false;
    }

    return true;
  }

  // See |ForEveryOwnedHierarchyPageInRange|. Each entry given to `T` is a VmPageOrMarkerRef, which
  // supports limited mutation.
  template <typename T>
  zx_status_t ForEveryOwnedMutableHierarchyPageInRangeLocked(T func, uint64_t offset, uint64_t size,
                                                             const LockedPtr& parent)
      TA_REQ(lock());

  // See |ForEveryOwnedHierarchyPageInRange|. Each entry given to `T` is mutable and `T` may modify
  // it or replace it with an empty entry.
  template <typename T>
  zx_status_t RemoveOwnedHierarchyPagesInRangeLocked(T func, uint64_t offset, uint64_t size,
                                                     const LockedPtr& parent) TA_REQ(lock());

  // Iterates a range within a visible node, invoking a callback for every `VmPageListEntry` the
  // node owns (fully or partially) in that range.
  //
  // The callback is invoked at most once per each offset within the range, as the node can own at
  // most one entry at each offset. Either:
  //  * The node directly contains the first visible entry at the offset and thus fully owns it.
  //  * A hidden parent contains the first visible entry at the offset and thus the node partially
  //    owns it.
  //  * A visible parent contains the first visible entry at the offset and thus that parent fully
  //    owns it. This method doesn't evaluate such parents and skips the offset.
  //  * There is no visible entry at the offset. This method skips the offset.
  //
  // Prefer using the non-static methods above over invoking this function directly.
  //
  // The caller provides:
  //  * `self`: Node to begin the iteration from. It must be a visible node.
  //  * `func`: Callback function invoked for each non-empty entry.
  //  * `offset`: Offset relative to `self` to begin iterating at.
  //  * `size`: Size of the range to iterate.
  //  * `parent`: If the caller has locked the immediate parent, then it can pass it in here to
  //              avoid double locking, otherwise if no parent or not locked a nullptr can be given.
  //
  // The type `S` must be implicitly convertible to a `VmCowPages` or a `const VmCowPages`.
  // The type `P` is `const VmPageOrMarker` if `S` is const, otherwise it is `VmPageOrMarker`.
  // The type `T` must be:
  // `zx_status_t(P* p, const VmCowPages* owner, uint64_t self_offset, uint64_t owner_offset)`
  //  The return value controls whether iteration continues:
  //   * `ZX_ERR_NEXT`: Continue iteration or stop with `ZX_OK` if no more entries to iterate.
  //   * `ZX_ERR_STOP`: Stop iteration immediately with `ZX_OK`.
  //   * Any other code: Stop iteration immediately with that error code.
  //
  // Returns `ZX_OK` if no `func` invocations returned any errors, otherwise returns the error code
  // from the failing `func` invocation.
  template <typename P, typename S, typename T>
  static zx_status_t ForEveryOwnedHierarchyPageInRange(S* self, T func, uint64_t offset,
                                                       uint64_t size, const LockedPtr& parent)
      TA_REQ(self->lock());

  // Changes a Reference in the provided VmPageOrMarker into a real vm_page_t. The allocated page
  // is assumed to be for this VmCowPages, and so uses the |pmm_alloc_flags|, but it is not assumed
  // that the page_or_mark is actually yet in this page_list_, and so the allocated page is not
  // added to the page queues. It is the responsibility of the caller to add to the page queues if
  // the page_or_mark is not stack owned.
  // The |page_request| must be non-null if the |pmm_alloc_flags| allow for delayed allocation, in
  // which case this may return ZX_ERR_SHOULD_WAIT if the page_request is filled out.
  zx_status_t MakePageFromReference(VmPageOrMarkerRef page_or_mark,
                                    AnonymousPageRequest* page_request);

  // Replaces the Reference in VmPageOrMarker owned by this page_list_ for a real vm_page_t.
  // Unlike MakePageFromReference this updates the page queues to track the newly added page. Use
  // of |page_request| and implications on return value are the same as |MakePageFromReference|.
  zx_status_t ReplaceReferenceWithPageLocked(VmPageOrMarkerRef page_or_mark, uint64_t offset,
                                             AnonymousPageRequest* page_request) TA_REQ(lock());

  // Helper for allocating and initializing a loaned page.
  template <typename F>
  zx::result<vm_page_t*> AllocLoanedPage(F allocated);

  // Helper for allocating a page for this VMO. The allocated page is not yet initialized, and
  // InitializeVmPage must be called on it prior to use. Callers most likely want AllocPage instead,
  // with this method being useful in rare cases where you want to defer initialization till
  // AddNewPagesLocked or similar.
  zx_status_t AllocUninitializedPage(vm_page_t** page, AnonymousPageRequest* page_request);

  // Helper for removing a page from the PageQueues and adding to a deferred ops for later freeing.
  void RemovePageLocked(vm_page_t* page, DeferredOps& ops) TA_REQ(lock());

  // Helper class for managing a two-part add page transaction. This object allows adding a page to
  // be split into a check and allocation, which can fail, with the final insertion, which cannot
  // fail.
  //
  // The transaction state lives in Rust (vm_cow_pages.rs); this is a thin wrapper over Rust-owned
  // storage so that C++ callers can hold one across a Begin*/Complete pair.
  class AddPageTransaction {
   public:
    AddPageTransaction(AddPageTransaction&& other);
    ~AddPageTransaction();
    AddPageTransaction(const AddPageTransaction&) = delete;
    AddPageTransaction& operator=(const AddPageTransaction&) = delete;
    AddPageTransaction& operator=(AddPageTransaction&&) = delete;

   private:
    friend VmCowPages;
    // Leaves |storage_| uninitialized; only the Begin* forwarders construct one, and they always
    // have it initialized by Rust before it is destroyed.
    AddPageTransaction() = default;

    [[maybe_unused]] OpaqueStorage<16, 8> storage_;
  };

  // Performs initial checks and slot allocations for inserting a new page at the specified
  // |offset|.
  //
  // |overwrite| controls how the function handles pre-existing content at |offset|. If |overwrite|
  // does not permit replacing the content, ZX_ERR_ALREADY_EXISTS will be returned.
  //
  // On success the returned |AddPageTransaction| *must* be used in a call to
  // |CompleteAddPageLocked| (or cancelled from Rust).
  //
  // No other |page_list_| operations should be performed until the transaction is complete, and
  // starting or ending a transaction should be assumed to invalidate any page_list_ iterators or
  // slots that have been looked up unless the caller explicitly knows otherwise.
  [[nodiscard]] zx::result<AddPageTransaction> BeginAddPageLocked(uint64_t offset,
                                                                  CanOverwriteSlot overwrite)
      TA_REQ(lock());

  // Similar to |BeginAddPageLocked| except only the |overwrite| checks are performed and a ready to
  // use slot is provided by the caller. It is a requirement by the caller to ensure that |slot|:
  //  * Is for |offset| in this |page_list_|
  //  * Does not fall in the middle of an interval.
  // All other requirements of |BeginAddPageLocked| otherwise apply.
  [[nodiscard]] zx::result<AddPageTransaction> BeginAddPageWithSlotLocked(
      uint64_t offset, VmPageOrMarkerRef slot, CanOverwriteSlot overwrite) TA_REQ(lock());

  // Completes an add page transaction that had been started by inserting the provided page |p| into
  // the slot looked up in |transaction|. Once complete the transaction must not be used in any
  // other complete or cancel calls.
  //
  // |p| must not be Empty()
  //
  // This operation unmaps the corresponding offset from any existing mappings, unless |deferred| is
  // a |nullptr|, in which case it will skip updating mappings.
  //
  // Any previous content in the slot is returned and must be dealt with by the caller.
  [[nodiscard]] VmPageOrMarker CompleteAddPageLocked(AddPageTransaction& transaction,
                                                     VmPageOrMarker&& p, DeferredOps* deferred)
      TA_REQ(lock());

  // Add a page to the object at |offset|. This is just a wrapper around performing
  // |BeginAddPageLocked| and then |CompleteAddPageLocked|, with error handling to perform a free of
  // the page given in |p| should insertion fail. Otherwise see those methods for a description of
  // the parameters.
  zx::result<VmPageOrMarker> AddPageLocked(uint64_t offset, VmPageOrMarker&& p,
                                           CanOverwriteSlot overwrite, DeferredOps* deferred)
      TA_REQ(lock());

  // Unmaps and frees all the committed pages in the specified range.
  // Upon success the removed pages are placed in the DeferredOps freed list, and the number of such
  // pages is returned.
  //
  // Unlike DecommitRangeLocked(), this function only operates on |this| node, which must have no
  // parent.
  // |offset| must be page aligned. |len| must be less than or equal to |size_ - offset|. If |len|
  // is less than |size_ - offset| it must be page aligned.
  zx::result<uint64_t> UnmapAndFreePagesLocked(uint64_t offset, uint64_t len, DeferredOps& deferred)
      TA_REQ(lock());

  // internal check if any pages in a range are pinned
  bool AnyPagesPinnedLocked(uint64_t offset, size_t len) TA_REQ(lock());

  // Searches for the content for the page in |this| at |offset|. If the offset is already
  // populated in |this| then that page is returned with the |owner| unset, otherwise the
  // parent hierarchy is searched for any content. The result could be used to initialize a commit,
  // or compare an existing commit with the original. The initial content is a VMPLCursor and may
  // be invalid if there was no explicit initial content. How to interpret an absence of content,
  // whether it is zero or otherwise, is left up to the caller.
  //
  // If an ancestor has a committed page which corresponds to |offset|, returns a cursor with
  // |current()| as that page as well as a LockedPtr to the VmCowPages and offset which own the
  // page. If no ancestor has a committed page for the offset, returns a cursor with a |current()|
  // of nullptr as well as the VmCowPages/offset which need to be queried to populate the page. If
  // |this| needs to be queried to populate the page, |owner| is not set, otherwise it is set to the
  // ancestor that owns the page. The reason being that |this| will already be externally locked by
  // the caller, whereas an ancestor needs to be locked inside this function.
  //
  // The returned |visible_end| represents the size of the range in the owner for which it can be
  // assumed that no child has content for, although for which the content might be in yet a higher
  // up parent. This will always be a subset of the provided |max_owner_length|, which serves as a
  // bound for the calculation and so passing in a smaller |max_owner_length| can sometimes be more
  // efficient.
  // It is an error for the |max_owner_length| to be < kPageSize.
  struct PageLookup {
    VMPLCursor cursor;
    LockedPtr owner;
    uint64_t owner_offset = 0;
    uint64_t visible_end = 0;
  };
  PageLookup FindPageContentLocked(uint64_t offset, uint64_t max_owner_length) TA_REQ(lock());

  // Searches for the initial content, i.e. the content that would be used to initially populate the
  // page, of |this| at |offset|. Whether there is presently any content populated in |this| is
  // ignored, and if there is content then this will still return what would be used to re-populate
  // that slot.
  PageLookup FindInitialPageContentLocked(uint64_t offset) TA_REQ(lock());

  // Helper function for reducing the share count of content in a hidden node, and freeing it if it
  // is no longer referenced.
  //
  // This method assumes that the caller is overriding the slot in a child and that it will perform
  // any necessary range change updates etc.
  void DecrementCowContentShareCount(const VmPageOrMarker& content, uint64_t offset,
                                     ScopedPageFreedList& list, VmCompression* compression)
      TA_REQ(lock());

  // Helper struct which encapsulates a parent node along with a range and limit relative to it.
  struct ParentAndRange {
    LockedPtr parent;
    LockedPtr grandparent;
    uint64_t parent_offset;
    uint64_t parent_limit;
    uint64_t size;
  };

  // Helper function for |CreateCloneLocked|.
  //
  // Walks the hierarchy from the |this| node to the root and finds the most distant node which
  // could correctly be the parent for a new clone of this node.
  //
  // Computes a range and limit for the clone, relative to the final parent, such that the clone
  // can't see any more of that parent than it could if it was a direct clone of `this` node.
  //
  // For correctness the parent is either:
  //  * The last node encountered which satisfies `parent_must_be_hidden` if its true. The initial
  //    `parent` node satisfies `parent_must_be_hidden` even though its visible, because it may be
  //    converted to a hidden node by the caller.
  //  * The first node encountered that has pages in the clone's range for it to snapshot.
  //  * The root.
  //
  // The caller provides:
  //  * `this`: Initial candidate parent node to begin the search from. It must be a visible node.
  //             The new clone is logically a clone of it.
  //  * `offset`: Offset of the clone relative to the initial parent.
  //  * `size`: Size of the clone.
  //  * `parent_must_be_hidden`: true iff the final parent must satisfy this constraint.
  //
  // Returns the actual node that should be used as the parent of the clone, and if that node has a
  // parent also returns a locked reference to that node. By locking the parent of the target parent
  // the caller ensures that any reasoning that made the target valid remains valid until the clone
  // can be created. As a result it is only valid to create the clone if done so whilst continuously
  // holding the returned locks.
  ParentAndRange FindParentAndRangeForCloneLocked(uint64_t offset, uint64_t size,
                                                  bool parent_must_be_hidden) TA_REQ(lock());

  // Helper function for |CreateCloneLocked|.
  //
  // Performs a clone by creating a new hidden parent, under which both the clone and this node will
  // hang. The range [|offset|,|offset| + |limit|) in this node become read-only and are
  // copy-on-write with the child. Anything in the |parent| above |this| in that range are also
  // copy-on-write with the child.
  //
  // If there is a parent_ then the passed in |parent| is a locked ptr to it.
  // An |initial_page_list| may be passed in to populate the clone's page list with any parent
  // content markers if needed. In this case, |initial_page_list_tracker| must be tracking the
  // number of populated slots in |initial_page_list|.
  zx::result<LockedRefPtr> CloneNewHiddenParentLocked(
      uint64_t offset, uint64_t limit, uint64_t size, VmPageList&& initial_page_list,
      AttributionTracker&& initial_page_list_tracker, const LockedPtr& parent) TA_REQ(lock());

  // Helper function for |CreateCloneLocked|.
  //
  // Performs a clone operation by hanging a new child under |this|.
  // Unlike |CloneNewHiddenParentLocked|, items in the range [|offset|,|offset| + |limit|) do not
  // become read-only, rather they remain writable by |this| and are copy-on-write in the child.
  // Anything in the |parent| above |this| in that range are also copy-in-write with the child.
  //
  // If there is a parent_ then the passed in |parent| is a locked ptr to it.
  // An |initial_page_list| may be passed in to populate the clone's page list with any parent
  // content markers if needed. In this case, |initial_page_list_tracker| must be tracking the
  // number of populated slots in |initial_page_list|.
  zx::result<LockedRefPtr> CloneChildLocked(uint64_t offset, uint64_t limit, uint64_t size,
                                            VmPageList&& initial_page_list,
                                            AttributionTracker&& initial_page_list_tracker,
                                            const LockedPtr& parent) TA_REQ(lock());

  // Release any pages this VMO can reference from the provided start offset till the end of the
  // VMO. This releases both directly owned pages, as well as pages in hidden parents that may be
  // considered owned by this VMO.
  // If applicable this method will update the parent_limit_ to reflect that it has removed any
  // reference to its parent range, and it can be assumed that upon return that
  // |parent_limit_ <= start|.
  // The caller is responsible for actually freeing the pages, which are returned in freed_list.
  // If the caller has locked the immediate parent, then it can pass it in as |parent| to avoid
  // double locking, otherwise if no parent or not locked a nullptr can be given.
  void ReleaseOwnedPagesLocked(uint64_t start, const LockedPtr& parent,
                               ScopedPageFreedList& freed_list) TA_REQ(lock()) {
    ReleaseOwnedPagesRangeLocked(start, size_locked() - start, parent, freed_list);
  }

  // Fast path for releasing all owned pages of a root VmCowPages (with no parent_).
  __ALWAYS_INLINE void ReleaseAllOwnedPagesRootLocked(ScopedPageFreedList& freed_list)
      TA_REQ(lock());

  // Similar to |ReleaseOwnedPagesLocked|, but only releases the specified range, and as such
  // provides no guarantee on the value of |parent_limit_|, with respect to |offset|, requiring the
  // caller to handle any issues with potential visibility into the parent range and content that
  // should no longer be referenced. More specifically, whereupon the conclusion of
  // |ReleaseOwnedPagesLocked| it can be assumed that |parent_limit_ <= start| it CANNOT be assumed
  // that |parent_limit_ <= offset|.
  void ReleaseOwnedPagesRangeLocked(uint64_t offset, uint64_t len, const LockedPtr& parent,
                                    ScopedPageFreedList& freed_list) TA_REQ(lock());

  // When cleaning up a hidden vmo, merges the hidden vmo's content (e.g. page list, view
  // of the parent) into the remaining child.
  // This can fail (return false) indicating content may have been partially merged. If this happens
  // the two vmos are in a valid state and can continue being used, but the parent must be retained
  // and cannot be deleted.
  bool MergeContentWithChildLocked() TA_REQ(lock());

  // Updates the page queue of an existing non-pinned page, moving it to whichever queue is
  // appropriate.
  void MoveToNotPinnedLocked(vm_page_t* page, uint64_t offset) TA_REQ(lock());

  // Places a newly added, not yet pinned, page into the appropriate page queue.
  void SetNotPinnedLocked(vm_page_t* page, uint64_t offset) TA_REQ(lock());

  // Updates the page's dirty state to the one specified, and also moves the page between page
  // queues if required by the dirty state. |dirty_state| should be a valid dirty tracking state,
  // i.e. one of Clean, AwaitingClean, or Dirty.
  //
  // |offset| is the page-aligned offset of the page in this object.
  //
  // |is_pending_add| indicates whether this page is yet to be added to this object's page list,
  // false by default. If the page is yet to be added, this function will skip updating the page
  // queue as an optimization, since the page queue will be updated later when the page gets added
  // to the page list. |is_pending_add| also helps determine certain validation checks that can be
  // performed on the page.
  void UpdateDirtyStateLocked(vm_page_t* page, uint64_t offset, DirtyState dirty_state,
                              bool is_pending_add = false) TA_REQ(lock());

  // Helper to invalidate any DIRTY requests in the specified range by spuriously resolving them.
  void InvalidateDirtyRequestsLocked(uint64_t offset, uint64_t len) TA_REQ(lock());

  // Updates direct children's parent limits after a shrinking Resize drops lock().
  // TODO(https://fxbug.dev/531878739): Remove once TreeWalkCursor is ported to Rust.
  void ResizeUpdateChildLimits(uint64_t s) TA_EXCL(lock());

  // Removes the specified child from this objects |children_list_| and performs any hierarchy
  // updates that need to happen as a result. This does not modify the |parent_| member of the
  // removed child and if this is not being called due to |removed| being destructed it is the
  // callers responsibility to correct parent_.
  // If |removed| has a sibling to its right (i.e. next in the children_list_) then |sibling| must
  // be a locked pointer to it. The exception being if this is a hidden node with two children, in
  // which case if |removed| is the right child then |sibling| should be set to the left child.
  __NO_INLINE void RemoveChildLocked(VmCowPages* removed, const LockedPtr& sibling) TA_REQ(lock())
      TA_REQ(removed->lock());

  // Inserts a newly created VmCowPages into this hierarchy as a child of this VmCowPages.
  // Initializes child members based on the passed in values that only have meaning when an object
  // is a child. This updates the parent_ field in child to hold a refptr to |this|.
  void AddChildLocked(VmCowPages* child, uint64_t offset, uint64_t parent_limit) TA_REQ(lock())
      TA_REQ(child->lock());

  void ReplaceChildLocked(VmCowPages* old, VmCowPages* new_child) TA_REQ(lock());

  void DropChildLocked(VmCowPages* c) TA_REQ(lock());

  // If page is still at offset, replace it with a different page.  If with_loaned is true, replace
  // with a loaned page.  If with_loaned is false, replace with a non-loaned page and a page_request
  // is required to be provided.
  zx_status_t ReplacePageLocked(vm_page_t* before_page, uint64_t offset, bool with_loaned,
                                vm_page_t** after_page, DeferredOps& deferred,
                                AnonymousPageRequest* page_request) TA_REQ(lock());

  // Copies the metadata information (dirty state, split bit information etc) from src_page to
  // dst_page in preparation for replacing src with dst. This copies the metadata information only
  // and not the contents, that must be done using the |CopyPageContentsForReplacementLocked|
  // method. This is split into two steps to allow the copying of the metadata and installation into
  // the page queues to be done under the pmm loaned pages lock, and then the copying of the page
  // contents to be done after with the loaned pages lock dropped.
  void CopyPageMetadataForReplacementLocked(vm_page_t* dst_page, vm_page_t* src_page)
      TA_REQ(lock());

  // Copies the page contents from src_page->dst_page to complete the replacement process. Typical
  // usage would have |CopyPageMetadatForReplacementLocked| already be performed, but this method
  // does not require it.
  void CopyPageContentsForReplacementLocked(vm_page_t* dst_page, vm_page_t* src_page)
      TA_REQ(lock());

  // Internal helper for performing reclamation via compression on an anonymous VMO. Assumes that
  // the provided |compressor| is not-null.
  VmCowReclaimResult ReclaimPageForCompression(vm_page_t* page, uint64_t offset,
                                               VmCompressor* compressor);

  // Internal helper for performing reclamation against a discardable VMO. If any discarding happens
  // the number of pages is returned. The passed in |page| must be the first page in the discardable
  // VMO to trigger a discard, otherwise it will fail.
  zx::result<uint64_t> ReclaimDiscardable(vm_page_t* page, uint64_t offset);

  // Internal helper for discarding a VMO. Will discard if VMO is unlocked returning the count.
  zx::result<uint64_t> DiscardPagesLocked(DeferredOps& deferred) TA_REQ(lock());

  // Applies the specific operation to all mappings in the given range against descendants/cow
  // children. The operation is not applied for this object. Only the DeferredOps is expected to
  // call this.
  // Takes ownership, and will drop, the lock for this object as children are iterated.
  static void RangeChangeUpdateCowChildren(LockedPtr self, VmCowRange range, RangeChangeOp op);

  uint32_t pmm_alloc_flags() const;
  VmCowPagesOptions options() const;
  void AssertCanary() const;
  OpaqueStorage<kVmCowPagesStateSize, kVmCowPagesStateAlign> opaque_storage_;

  mutable LOCK_DEP_INSTRUMENT(VmCowPages, CriticalMutex, lockdep::LockFlagsNestable) lock_;

  // When acquiring multiple locks they must be acquired in order from lowest to highest. To support
  // unidirectional clones, where nodes gain new children, and bidirectional clones, where nodes
  // gain new parents, lock ordering is determined using the following scheme:
  //  * A node with a page source, as it will always be the root, is given the highest order of
  //    kLockOrderRoot.
  //  * The first anonymous node in a chain is given the a lock order in the middle of
  //    kLockOrderFirstAnon. This is nodes such as:
  //    - Direct child of a root page source node.
  //    - Direct Child of a hidden node.
  //    - New anonymous root node.
  //  * Children of visible anonymous nodes, i.e. unidirectional clones of a non-hidden non pager
  //    backed node, take their parents lock order minus the kLockOrderDelta.
  //  * Hidden nodes take either kLockOrderRoot, if they are becoming the root node, or their
  //    parents lock order minus the kLockOrderDelta.
  // The goal of this scheme is to provide room in the numbering for both unidirectional children
  // to grow down at the bottom, and hidden nodes to grow down in the middle, without colliding. If
  // children of hidden nodes did not start at kLockOrderFirstAnon, but instead just took a minimum
  // lock order, then a collision would occur if:
  //  1. A pager backed node is created that then has a hidden node below it, with two anonymous
  //     leaf nodes below it.
  //  2. A new clone is created from one of those leafs that can hang directly off the hidden node.
  //  3. Both the original leaf nodes are closed, merging the remaining child with the hidden node.
  //  4. A unidirectional clone is now created from what is now a unidirectional hierarchy.
  // Here, space is needed to grow down, as we have effectively found a way to promote a leaf child
  // of a hidden node to being part of a unidirectional clone chain.
  //
  // Having a non-contiguous numbering allows for using an alternate lock ordering scheme during
  // clone construction and dead transitions. When creating new nodes since there are no other
  // references the lock cannot be held and so we cannot deadlock. However we still need to provide
  // a lock order to satisfy lockdep. Here the gaps created by kLockOrderDelta can be used as the
  // order for these newly created nodes.
  //
  // During a dead transition we potentially need to hold locks of three nodes: the parent node and
  // two of its children. Here the order is that the children must be acquired in list order, and
  // then the parent. When acquiring the second child, since its lock order would be equal to the
  // first child, the guaranteed gap between the first child and the parent lock order is used
  // instead.
  static constexpr uint64_t kLockOrderDelta = 3;
  static constexpr uint64_t kLockOrderRoot = UINT64_MAX - kLockOrderDelta;
  static constexpr uint64_t kLockOrderFirstAnon = UINT64_MAX / 2;

  // Parent of this node (may be null). The reference itself is owned by the Rust state.
  VmCowPages* parent_locked() const TA_REQ(lock());
  fbl::RefPtr<VmCowPages> take_parent_locked() TA_REQ(lock());
  void set_parent_locked(fbl::RefPtr<VmCowPages> parent) TA_REQ(lock());
  // The page source, if any.
  PageSource* page_source() const;

  // list of every child
  fbl::TaggedDoublyLinkedList<VmCowPages*, internal::ChildListTag> children_list_
      TA_GUARDED(lock());

  // To support iterating over a subtree a cursor object is used and installed in nodes as they are
  // iterated. This ensures that if iteration races with any node destruction that the cursor can be
  // used to perform fixups.
  // Any cursors in these lists are processed (i.e. moved) during a dead transition, and so it is
  // invalid to perform an iteration over a non-alive node / subtree. Equivalently the cursor itself
  // relies on this fact to allow it to safely store raw pointer backlinks, knowing they will always
  // be cleared in a dead transition prior to the pointer becoming invalid.
  // Both the root (i.e. start and final termination point) and the current location of any cursor
  // needs to be tracked, as these both need potential updates.
  struct RootListTag {};
  struct CurListTag {};
  class TreeWalkCursor;
  fbl::TaggedDoublyLinkedList<TreeWalkCursor*, RootListTag> root_cursor_list_ TA_GUARDED(lock());
  fbl::TaggedDoublyLinkedList<TreeWalkCursor*, CurListTag> cur_cursor_list_ TA_GUARDED(lock());

  VmPageList& page_list_locked() TA_REQ(lock());
  const VmPageList& page_list_locked() const TA_REQ(lock());

  AttributionTracker& continuous_attribution_tracker_locked() TA_REQ(lock());
  const AttributionTracker& continuous_attribution_tracker_locked() const TA_REQ(lock());

  // Reference back to a VmObjectPaged, which should be valid at all times after creation until the
  // VmObjectPaged has been destroyed, unless this is a hidden node. We use this in places where we
  // have access to the VmCowPages and need to look up the "owning" VmObjectPaged for some
  // information, e.g. when deduping zero pages, for performing cache or mapping updates, for
  // inserting references to the reference list.
  //
  // This is a raw pointer to avoid circular references, the VmObjectPaged destructor needs to
  // update it.
  VmObjectPaged* paged_ref_ TA_GUARDED(lock()) = nullptr;

  // Non-null if this is a discardable VMO.
  const ktl::unique_ptr<DiscardableVmoTracker> discardable_tracker_;

  // Tracks the life cycle of the VmCowPages. The primary purpose of the life cycle is to create an
  // invariant that by the time a VmCowPages destructor runs it does not contain any pages. This is
  // achieved by requiring an explicit Dead transition that provides a point to perform cleanup.
  // An Init state is introduced to allow for multi step creation that may fail.
  enum class LifeCycle : uint8_t {
    Init,
    Alive,
    Dying,
    Dead,
  };

  uint64_t parent_offset_locked() const TA_REQ(lock());
  void set_parent_offset_locked(uint64_t v) TA_REQ(lock());
  uint64_t parent_limit_locked() const TA_REQ(lock());
  void set_parent_limit_locked(uint64_t v) TA_REQ(lock());
  uint64_t root_parent_offset_locked() const TA_REQ(lock());
  void set_root_parent_offset_locked(uint64_t v) TA_REQ(lock());

  uint32_t children_list_len_locked() const TA_REQ(lock());
  void set_children_list_len_locked(uint32_t v) TA_REQ(lock());
  void set_reclamation_event_count_locked(uint64_t v) TA_REQ(lock());
  int64_t high_priority_count_locked() const TA_REQ(lock());
  void set_high_priority_count_locked(int64_t v) TA_REQ(lock());
  bool pager_stats_modified_locked() const TA_REQ(lock());
  void set_pager_stats_modified_locked(bool v) TA_REQ(lock());
  LifeCycle life_cycle_locked() const TA_REQ(lock());
  void set_life_cycle_locked(LifeCycle v) TA_REQ(lock());
  bool ever_pinned() const;
  bool ever_pinned_locked() const TA_REQ(lock());
  void set_ever_pinned_locked(bool v) TA_REQ(lock());
};

// Implements a cursor that allows for retrieving successive pages over a range in a VMO. The
// range that is iterated is determined at construction from GetLookupCursorLocked and cannot be
// modified, although it can be effectively shrunk by ceasing queries early.
//
// The cursor is designed under the assumption that the caller is tracking, implicitly or
// explicitly, how many queries have been done, and the methods do not return errors if more slots
// are queried than was originally requested in the range. They will, however, assert and panic.
//
// There are two controls provided by this object.
//
//   Zero forks: By default new zero pages will be considered zero forks and added to the zero page
//   scanner list, this can be disabled with |DisableZeroFork|.
//
//   Access time: By default pages that are returned will be considered accessed. This can be
//   changed with |DisableMarkAccessed|.
//
// The VMO lock *must* be held contiguously from the call to GetLookupCursorLocked over the entire
// usage of this object. Callers can assume that for any vm_page_t* that is returned that the lock
// of the owner of that page is held up until the next operation performed on the cursor.
//
// The cursor state and implementation live in Rust (vm_cow_pages.rs); this is a thin wrapper over
// Rust-owned storage for the callers that are still C++, and only has the methods they use. As the
// Rust cursor is address sensitive this cannot be copied or moved, and is constructed in place.
class VmCowPages::LookupCursor {
 public:
  ~LookupCursor();

  // Convenience struct holding the return result of the Require* methods.
  struct RequireResult {
    vm_page_t* page = nullptr;
    bool writable = false;
  };

  // The Require* methods will attempt to lookup the next offset in the VMO and return you a page
  // with the properties requested. If a page can be returned in the zx::ok result then the internal
  // cursor is incremented and future operations will act on the next offset. If an error occurs
  // then the internal cursor is not incremented.
  // These methods all take a PageRequest, which will be populated in the case of returning
  // ZX_ERR_SHOULD_WAIT. For optimal page request generation the |max_request_pages| controls how
  // many pages you are intending to lookup, and |max_request_pages| must not exceed the remaining
  // window of the cursor.
  // The returned page, unless it was just allocated, will have its access time updated based on
  // |EnableMarkAccessed|, with newly allocated pages always being default considered to have just
  // been accessed.

  // Returned page must be an allocated and owned page in this VMO. As such this will never return a
  // reference to the zero page. |will_write| indicates if this page needs to be writable or not,
  // which for an owned and allocated page just involves a potential dirty request / transition.
  zx::result<RequireResult> RequireOwnedPage(bool will_write, uint64_t max_request_pages,
                                             DeferredOps& deferred, MultiPageRequest* page_request)
      TA_REQ(lock());

  // Returned page will be readable or writable based on the |will_write| flag.
  zx::result<RequireResult> RequirePage(bool will_write, uint64_t max_request_pages,
                                        DeferredOps& deferred, MultiPageRequest* page_request)
      TA_REQ(lock());

  // Checks the current slot for a page and returns it. This does not return zero pages and, due to
  // the lack of taking a page request, will not perform copy-on-write allocations or dirty
  // transitions. In these cases it will return nullptr even though there is content.
  // The internal cursor is always incremented regardless of the return value.
  vm_page_t* MaybePage(bool will_write) TA_REQ(lock());

  // Disables placing newly allocated zero pages in the zero fork list.
  void DisableZeroFork();

  // Indicates that any existing pages that are returned should not be considered accessed and have
  // their accessed times updated.
  void DisableMarkAccessed();

  // Exposed for lock assertions.
  Lock<CriticalMutex>* lock() const TA_RET_CAP(target()->lock_ref()) { return target()->lock(); }
  Lock<CriticalMutex>& lock_ref() const TA_RET_CAP(target()->lock_ref()) {
    return target()->lock_ref();
  }

  LookupCursor(const LookupCursor&) = delete;
  LookupCursor(LookupCursor&&) = delete;
  LookupCursor& operator=(const LookupCursor&) = delete;
  LookupCursor& operator=(LookupCursor&&) = delete;

  // The lock of |target| must be held from construction until the cursor is destroyed.
  LookupCursor(VmCowPages* target, VmCowRange range);

 private:
  // The target the cursor was constructed with.
  VmCowPages* target() const;

  // Storage for the Rust LookupCursor, whose size depends on whether its LockedPtr carries lockdep
  // state.
#if defined(LOCK_DEP_ENABLED_FEATURE_LEVEL) && LOCK_DEP_ENABLED_FEATURE_LEVEL >= 2
  [[maybe_unused]] OpaqueStorage<160, 8> storage_;
#else
  [[maybe_unused]] OpaqueStorage<112, 8> storage_;
#endif
};

class ScopedPageFreedList {
 public:
  explicit ScopedPageFreedList() { rust_scoped_page_freed_list_init(this); }

  ~ScopedPageFreedList() { rust_scoped_page_freed_list_destroy(this); }

  ScopedPageFreedList(const ScopedPageFreedList&) = delete;
  ScopedPageFreedList(ScopedPageFreedList&&) = delete;
  ScopedPageFreedList& operator=(const ScopedPageFreedList&) = delete;
  ScopedPageFreedList& operator=(ScopedPageFreedList&&) = delete;

  void FreePages(VmCowPages* cow_pages) { rust_scoped_page_freed_list_free_pages(this, cow_pages); }
  VmPageDoublyLinkedList* List() { return rust_scoped_page_freed_list_list(this); }
  FreeLoanedPagesHolder& Flph() { return *rust_scoped_page_freed_list_flph(this); }

 private:
  [[maybe_unused]] OpaqueStorage<40, 8> storage_{};
};

// Helper object for finishing VmCowPages operations that must occur after the lock is dropped. This
// is necessary due to some operations being externally locked. It is expected that this object is
// stack allocated using the __UNINITIALIZED tag in a sequence like this:
//
//     __UNINITIALIZED VmCowPages::DeferredOps deferred(cow_object_);
//     Guard<CriticalMutex> guard{cow_object_->lock()};
//     cow_object_->DoOperationLocked(&deferred);
//
// The destruction order will then allow |deferred| to perform its actions after |guard| is
// destructed and the lock is dropped.
// This class it not thread safe.
class VmCowPages::DeferredOps {
 public:
  // Construct a DeferredOps for the given VmCowPages. Must be constructed, and deconstructed,
  // without the lock held. It is the callers responsibility to ensure the pointer remains valid
  // over the lifetime of the object.
  explicit DeferredOps(VmCowPages* self) TA_EXCL(self->lock());
  ~DeferredOps();

  DeferredOps(const DeferredOps&) = delete;
  DeferredOps(DeferredOps&&) = delete;
  DeferredOps& operator=(const DeferredOps&) = delete;
  DeferredOps& operator=(DeferredOps&&) = delete;

 private:
  // Methods are private as they are only intended for use by the VmCowPages and not the external
  // caller holding this object on their stack.
  friend VmCowPages;
  friend PriorityChanger;

  // Retrieves the underlying resource containers. Any pages (loaned or otherwise) that are added
  // will be freed *after* any range change operations are first performed.
  ScopedPageFreedList& FreedList(VmCowPages* self);

  const VmCowPages* self() const;

#if defined(LOCK_DEP_ENABLED_FEATURE_LEVEL) && LOCK_DEP_ENABLED_FEATURE_LEVEL >= 2
  [[maybe_unused]] OpaqueStorage<128, 8> storage_{};
#else
  [[maybe_unused]] OpaqueStorage<88, 8> storage_{};
#endif
};

// PriorityChanger is a transaction object for changing the high priority count of VmCowPages
// objects. The high priority count is used to manage page reclamation. This object ensures that a
// DeferredOps object is created against the correct VmCowPages, which might be an ancestor of
// |target_|. PriorityChanger does the following:
//
// * Manages the lifetime of VmCowPages to ensure they last until the
// priority change operation, by holding a RefPtr.
// * Handles the construction of the DeferredOps object.
// * Clarifies which operations need to be done within the VmCowPages lock and which need to happen
// without the lock being held.
// * Presents a high-level API to VmMapping and VmObjectPaged.
// * Handles traversing up the VmCowPages hierarchy to ensure that high priority status is
// propagated.
//
// Callers should prefer |VmMapping::set_memory_priority_locked|,
// |VmMapping::set_memory_priority_default_locked_object|, or
// |VmMapping::set_memory_priority_high_already_positive_locked_object|, if applicable.
//
// To change the high priority count of a VmCowPages, you should first construct the PriorityChanger
// object. Then,
// * If |delta| < 0, you must immediately call one of the Change* methods, which change the high
// priority count of |target|.
// * If |delta| > 0, you must call an applicable Prepare* method to set up the prerequisites for the
// priority change operation, then call one of the Change* methods.
//
// This class is not thread-safe.
class PriorityChanger {
 public:
  // Construct a priority changer to change the high priority count of |target|. When |delta| is
  // positive, we go towards high priority. When it's negative, we move towards default
  // priority.
  //
  // The constructor may be called with or without |target|'s lock being held.
  //
  // The caller must ensure that |target| remains valid during the lifetime of PriorityChanger.
  PriorityChanger(int64_t delta, VmCowPages* target);

  ~PriorityChanger() {
    // Setting up a PriorityChanger without using it is wasteful.
    DEBUG_ASSERT(state_ == PriorityChangerState::DONE);
  }

  DISALLOW_COPY_ASSIGN_AND_MOVE(PriorityChanger);

  Lock<CriticalMutex>* lock() const TA_RET_CAP(target_->lock()) { return target_->lock(); }
  Lock<CriticalMutex>& lock_ref() const TA_RET_CAP(target_->lock()) { return target_->lock_ref(); }

  // This method may be called if |target|'s root page source has no loaned pages. Prefer other
  // Prepare* methods as this is typically hard to reason about.
  //
  // If you call this method, please leave a comment explaining how you know there are no loaned
  // pages in the current VmObject hierarchy.
  //
  // This method must be called with |target|'s lock held.
  void PrepareHasNoLoanedPagesLocked() TA_REQ(lock());

  // This method may be called if |target| already has a |high_priority_count_| > 0.
  //
  // This method must be called with |target|'s lock held.
  void PrepareIsAlreadyHighPriorityLocked() TA_REQ(lock());

  // This method must be called *without* holding |target|'s VmCowPages lock. In addition, the
  // caller must not hold a PriorityChanger or DeferredOps on any other VmCowPages in |target|'s
  // hierarchy.
  void PrepareMayNotAlreadyBeHighPriority() TA_EXCL(lock());

  // Increments or decrements the priority count of this VMO. The high priority count is used to
  // control any page reclamation, and applies to the whole VMO, including its parents. The count is
  // never allowed to go negative and so callers must only subtract what they have already added.
  // Further, callers are required to remove any additions before the VMO is destroyed.
  void ChangeHighPriorityCountLocked() TA_REQ(lock());

  // Internal (to VmCowPages) helper for modifying just this value of |high_priority_count_| without
  // performing any propagating.
  void ChangeSingleHighPriorityCountLocked() TA_REQ(lock());

 private:
  enum class PriorityChangerState : uint8_t {
    // User must call one of the Prepare* methods next.
    CONSTRUCTED,
    // User can call one of the Change* methods next.
    READY_TO_CHANGE,
    // User already called Change* so the only thing we can do is call the destructor.
    DONE
  };

  // Helper for PriorityChanger::ChangeSingleHighPriorityCountLocked and
  // PriorityChanger::ChangeHighPriorityCountLocked. Returns a new delta for whatever priority
  // change needs to be applied to the parent. With a zero return value, propagation must be
  // halted.
  //
  // This method is called with |current| starting as |target_|, and may eventually be called with
  // |current| as |root_|.
  //
  // Provide root_deferred whenever there may be loaned pages in the root.
  //
  // This method is static so that we can keep delta_ const, and propagate the change by using the
  // return value.
  static int64_t ChangeSingleHighPriorityCountLockedHelper(
      VmCowPages& current, int64_t delta, ktl::optional<VmCowPages::DeferredOps>& root_deferred)
      TA_REQ(current.lock());

  PriorityChangerState state_ = PriorityChangerState::CONSTRUCTED;
  // When delta_ > 0, move towards high priority, when delta_ < 0, move towards default priority.
  const int64_t delta_;
  // This must be present whenever root_deferred_ is present, since we are holding the VmCowPages to
  // ensure it stays live while root_deferred_ is live.
  fbl::RefPtr<VmCowPages> root_;
  // Always non-null.
  VmCowPages* const target_;
  // Must be present when there are loaned pages.
  ktl::optional<VmCowPages::DeferredOps> root_deferred_;
};

// What the Rust LookupCursor Require* methods return to the C++ LookupCursor: a
// zx::result<RequireResult> flattened so that it is returned in two registers. |page| and
// |writable| are only meaningful if |status| is ZX_OK.
//
// |page| is a uintptr_t and not a vm_page_t*: on x86-64 Clang lowers a {pointer, integer} struct to
// `{ ptr, i64 }` and rustc lowers the Rust mirror to `{ i64, i64 }`, and the mismatch stops
// cross-language inlining of the rust_vm_cow_pages_lookup_cursor_require_* trampolines.
// TODO(https://fxbug.dev/537458631): Make this a vm_page_t* once cross-language inlining works.
struct VmCowPagesLookupCursorRequireResult {
  uintptr_t page;
  zx_status_t status;
  bool writable;
};

// What rust_vm_cow_pages_zero_pages_locked returns to VmCowPages::ZeroPagesLocked: a
// ktl::pair<zx_status_t, uint64_t> flattened so that it is returned in two registers.
struct VmCowPagesZeroPagesResult {
  zx_status_t status;
  uint64_t zeroed_len;
};

extern "C" {
void rust_vm_cow_pages_state_init(void* storage, uint32_t pmm_alloc_flags, uint32_t options,
                                  uint64_t size, PageSource* page_source, uint64_t lock_order);
void rust_vm_cow_pages_destruct(void* storage);
void rust_vm_cow_pages_assert_canary(const VmCowPages* cow);
__PURE uint32_t rust_vm_cow_pages_pmm_alloc_flags(const VmCowPages* cow);
__PURE uint32_t rust_vm_cow_pages_options(const VmCowPages* cow);
__PURE uint64_t rust_vm_cow_pages_size_locked(const VmCowPages* cow);
__PURE uint64_t rust_vm_cow_pages_parent_offset_locked(const VmCowPages* cow);
void rust_vm_cow_pages_set_parent_offset_locked(const VmCowPages* cow, uint64_t v);
__PURE uint64_t rust_vm_cow_pages_parent_limit_locked(const VmCowPages* cow);
void rust_vm_cow_pages_set_parent_limit_locked(const VmCowPages* cow, uint64_t v);
__PURE uint64_t rust_vm_cow_pages_root_parent_offset_locked(const VmCowPages* cow);
void rust_vm_cow_pages_set_root_parent_offset_locked(const VmCowPages* cow, uint64_t v);
VmPageList* rust_vm_cow_pages_page_list_locked(const VmCowPages* cow);
AttributionTracker* rust_vm_cow_pages_continuous_attribution_tracker_locked(const VmCowPages* cow);
__PURE uint64_t rust_vm_cow_pages_heap_allocation_bytes_locked(const VmCowPages* cow);
__PURE uint32_t rust_vm_cow_pages_children_list_len_locked(const VmCowPages* cow);
void rust_vm_cow_pages_set_children_list_len_locked(const VmCowPages* cow, uint32_t v);
__PURE uint64_t rust_vm_cow_pages_pinned_page_count_locked(const VmCowPages* cow);
__PURE uint64_t rust_vm_cow_pages_reclamation_event_count_locked(const VmCowPages* cow);
void rust_vm_cow_pages_set_reclamation_event_count_locked(const VmCowPages* cow, uint64_t v);
__PURE int64_t rust_vm_cow_pages_high_priority_count_locked(const VmCowPages* cow);
void rust_vm_cow_pages_set_high_priority_count_locked(const VmCowPages* cow, int64_t v);
__PURE bool rust_vm_cow_pages_pager_stats_modified_locked(const VmCowPages* cow);
void rust_vm_cow_pages_set_pager_stats_modified_locked(const VmCowPages* cow, bool v);
// LifeCycle is a private member of VmCowPages, so it crosses the FFI boundary as its underlying
// uint8_t; the private forwarders below cast to and from the enum.
__PURE uint8_t rust_vm_cow_pages_life_cycle_locked(const VmCowPages* cow);
void rust_vm_cow_pages_set_life_cycle_locked(const VmCowPages* cow, uint8_t v);
bool rust_vm_cow_pages_ever_pinned(const VmCowPages* cow);
__PURE bool rust_vm_cow_pages_ever_pinned_locked(const VmCowPages* cow);
void rust_vm_cow_pages_set_ever_pinned_locked(const VmCowPages* cow, bool v);
__PURE bool rust_vm_cow_pages_is_high_memory_priority_locked(const VmCowPages* cow);
__PURE bool rust_vm_cow_pages_has_no_children_locked(const VmCowPages* cow);
void rust_vm_cow_pages_transition_to_alive_locked(const VmCowPages* cow);
void rust_vm_cow_pages_reset_pager_vmo_stats_locked(const VmCowPages* cow);
__PURE bool rust_vm_cow_pages_is_hidden(const VmCowPages* cow);
__PURE bool rust_vm_cow_pages_can_decommit_zero_pages(const VmCowPages* cow);
zx_status_t rust_vm_cow_pages_query_pager_vmo_stats_locked(const VmCowPages* cow, bool reset,
                                                           zx_pager_vmo_stats_t* stats);
void rust_vm_cow_pages_move_to_not_pinned_locked(VmCowPages* cow, vm_page_t* page, uint64_t offset);
void rust_vm_cow_pages_set_not_pinned_locked(VmCowPages* cow, vm_page_t* page, uint64_t offset);
void rust_vm_cow_pages_update_dirty_state_locked(VmCowPages* cow, vm_page_t* page, uint64_t offset,
                                                 VmCowPages::DirtyState dirty_state,
                                                 bool is_pending_add);
void rust_vm_cow_pages_finish_cache_policy_transition_locked(VmCowPages* cow);
bool rust_vm_cow_pages_any_pages_pinned_locked(VmCowPages* cow, uint64_t offset, size_t len);
bool rust_vm_cow_pages_debug_is_range_pinned_locked(VmCowPages* cow, VmCowRange range);
// |lookup_fn| / |dirty_range_fn| point at the C++ VmObject::LookupFunction /
// DirtyRangeEnumerateFunction, which Rust invokes through cpp_vm_cow_pages_call_lookup_fn /
// cpp_vm_cow_pages_call_dirty_range_fn.
zx_status_t rust_vm_cow_pages_lookup_locked(const VmCowPages* cow, VmCowRange range,
                                            void* lookup_fn);
zx_status_t rust_vm_cow_pages_enumerate_dirty_ranges_locked(VmCowPages* cow, VmCowRange range,
                                                            void* dirty_range_fn);
bool rust_vm_cow_pages_debug_validate_continuous_attribution_locked(const VmCowPages* cow);
uint64_t rust_vm_cow_pages_debug_get_page_count_locked(const VmCowPages* cow);
bool rust_vm_cow_pages_debug_is_page(const VmCowPages* cow, uint64_t offset);
bool rust_vm_cow_pages_debug_is_marker(const VmCowPages* cow, uint64_t offset);
uint32_t rust_vm_cow_pages_debug_get_marker_share_count(const VmCowPages* cow, uint64_t offset);
bool rust_vm_cow_pages_debug_is_parent_content(const VmCowPages* cow, uint64_t offset);
bool rust_vm_cow_pages_debug_is_empty(const VmCowPages* cow, uint64_t offset);
vm_page_t* rust_vm_cow_pages_debug_get_page(const VmCowPages* cow, uint64_t offset);
vm_page_t* rust_vm_cow_pages_debug_get_page_locked(const VmCowPages* cow, uint64_t offset);
bool rust_vm_cow_pages_debug_is_high_memory_priority(const VmCowPages* cow);
uint32_t rust_vm_cow_pages_debug_get_populated_slots_count(const VmCowPages* cow);
PmmOptDelayReuse rust_vm_cow_pages_should_delay_reuse_on_free(const VmCowPages* cow);
void rust_vm_cow_pages_free_pages(VmCowPages* cow, VmPageDoublyLinkedList* pages);
void rust_vm_cow_pages_free_page(VmCowPages* cow, vm_page_t* page);
zx_status_t rust_vm_cow_pages_alloc_uninitialized_page(VmCowPages* cow, vm_page_t** page,
                                                       AnonymousPageRequest* page_request);
zx_status_t rust_vm_cow_pages_alloc_page(VmCowPages* cow, vm_page_t** page,
                                         AnonymousPageRequest* page_request);
void rust_vm_cow_pages_initialize_vm_page(vm_page_t* page);
zx_status_t rust_vm_cow_pages_make_page_from_reference(VmCowPages* cow,
                                                       VmPageOrMarkerRef page_or_mark,
                                                       AnonymousPageRequest* page_request);
zx_status_t rust_vm_cow_pages_replace_reference_with_page_locked(
    VmCowPages* cow, VmPageOrMarkerRef page_or_mark, uint64_t offset,
    AnonymousPageRequest* page_request);
void rust_vm_cow_pages_deferred_ops_init(VmCowPages::DeferredOps* ops, VmCowPages* cow);
void rust_vm_cow_pages_deferred_ops_destroy(VmCowPages::DeferredOps* ops);
ScopedPageFreedList* rust_vm_cow_pages_deferred_ops_freed_list(VmCowPages::DeferredOps* ops,
                                                               const VmCowPages* cow);
__PURE const VmCowPages* rust_vm_cow_pages_deferred_ops_self(const VmCowPages::DeferredOps* ops);
void rust_vm_cow_pages_remove_page_locked(const VmCowPages* cow, vm_page_t* page,
                                          VmCowPages::DeferredOps* ops);
void rust_vm_cow_pages_range_change_update_locked(const VmCowPages* cow, VmCowRange range,
                                                  VmObject::RangeChangeOp op,
                                                  VmCowPages::DeferredOps* deferred);
void rust_vm_cow_pages_range_change_update_mappings_locked(const VmCowPages* cow,
                                                           VmObjectPaged* paged, VmCowRange range,
                                                           VmObject::RangeChangeOp op);
// AddPageTransaction is a private member of VmCowPages, so it crosses the FFI boundary as void*;
// the private forwarders below pass the object's address. |out_transaction| is always initialized
// on return (to an inert transaction on failure) so that the C++ AddPageTransaction destructor is
// well defined on every path.
zx_status_t rust_vm_cow_pages_begin_add_page_with_slot_locked(
    VmCowPages* cow, uint64_t offset, VmPageOrMarkerRef slot,
    VmCowPages::CanOverwriteSlot overwrite, void* out_transaction);
zx_status_t rust_vm_cow_pages_begin_add_page_locked(VmCowPages* cow, uint64_t offset,
                                                    VmCowPages::CanOverwriteSlot overwrite,
                                                    void* out_transaction);
void rust_vm_cow_pages_add_page_transaction_move(void* dst, void* src);
void rust_vm_cow_pages_add_page_transaction_destroy(void* transaction);
void rust_vm_cow_pages_complete_add_page_locked(VmCowPages* cow, void* transaction,
                                                VmPageOrMarker* p,
                                                VmCowPages::DeferredOps* deferred,
                                                VmPageOrMarker* out_old);
// The range is passed as two scalars and not as a VmCowRange because Clang and rustc lower a
// by-value VmCowRange to different LLVM types on x86-64, stopping cross-language inlining.
// TODO(https://fxbug.dev/537458631): Pass a VmCowRange once cross-language inlining works.
void rust_vm_cow_pages_lookup_cursor_init(VmCowPages::LookupCursor* cursor, VmCowPages* target,
                                          uint64_t offset, uint64_t len);
void rust_vm_cow_pages_lookup_cursor_destroy(VmCowPages::LookupCursor* cursor);
__PURE VmCowPages* rust_vm_cow_pages_lookup_cursor_target(const VmCowPages::LookupCursor* cursor);
VmCowPagesLookupCursorRequireResult rust_vm_cow_pages_lookup_cursor_require_owned_page(
    VmCowPages::LookupCursor* cursor, bool will_write, uint64_t max_request_pages,
    VmCowPages::DeferredOps* deferred, MultiPageRequest* page_request);
VmCowPagesLookupCursorRequireResult rust_vm_cow_pages_lookup_cursor_require_page(
    VmCowPages::LookupCursor* cursor, bool will_write, uint64_t max_request_pages,
    VmCowPages::DeferredOps* deferred, MultiPageRequest* page_request);
vm_page_t* rust_vm_cow_pages_lookup_cursor_maybe_page(VmCowPages::LookupCursor* cursor,
                                                      bool will_write);
void rust_vm_cow_pages_lookup_cursor_disable_zero_fork(VmCowPages::LookupCursor* cursor);
void rust_vm_cow_pages_lookup_cursor_disable_mark_accessed(VmCowPages::LookupCursor* cursor);
zx_status_t rust_vm_cow_pages_add_new_page_locked(VmCowPages* cow, uint64_t offset, vm_page_t* page,
                                                  VmCowPages::CanOverwriteSlot overwrite,
                                                  VmPageOrMarker* released_page, bool zero,
                                                  VmCowPages::DeferredOps* deferred);
zx_status_t rust_vm_cow_pages_add_new_pages_locked(VmCowPages* cow, uint64_t start_offset,
                                                   VmPageDoublyLinkedList* pages,
                                                   VmCowPages::CanOverwriteSlot overwrite,
                                                   bool zero, VmCowPages::DeferredOps* deferred);
zx_status_t rust_vm_cow_pages_decommit_range(VmCowPages* cow, VmCowRange range);
void rust_vm_cow_pages_get_attributed_memory_in_range_locked(
    const VmCowPages* cow, VmCowRange range, VmCowPages::AttributionCounts* out_counts);
uint8_t rust_vm_cow_pages_page_would_read_zero_locked(VmCowPages* cow, uint64_t page_offset);
zx_status_t rust_vm_cow_pages_lookup_readable_locked(VmCowPages* cow, VmCowRange range,
                                                     void* lookup_fn);
zx_status_t rust_vm_cow_pages_prepare_for_write_locked(VmCowPages* cow, VmCowRange range,
                                                       LazyPageRequest* page_request,
                                                       uint64_t* dirty_len_out);
zx_status_t rust_vm_cow_pages_fail_page_requests_locked(VmCowPages* cow, VmCowRange range,
                                                        zx_status_t error_status);
zx_status_t rust_vm_cow_pages_writeback_begin_locked(VmCowPages* cow, VmCowRange range,
                                                     bool is_zero_range);
zx_status_t rust_vm_cow_pages_writeback_end_locked(VmCowPages* cow, VmCowRange range);
void rust_vm_cow_pages_detach_source(VmCowPages* cow);
zx_status_t rust_vm_cow_pages_unmap_and_free_pages_locked(VmCowPages* cow, uint64_t offset,
                                                          uint64_t len,
                                                          VmCowPages::DeferredOps* deferred,
                                                          uint64_t* out_freed_count);
zx_status_t rust_vm_cow_pages_pin_range_locked(VmCowPages* cow, VmCowRange range);
void rust_vm_cow_pages_unpin_locked(VmCowPages* cow, VmCowRange range,
                                    VmCowPages::DeferredOps* deferred);
zx_status_t rust_vm_cow_pages_lock_range_locked(VmCowPages* cow, VmCowRange range,
                                                zx_vmo_lock_state_t* lock_state_out);
zx_status_t rust_vm_cow_pages_try_lock_range_locked(VmCowPages* cow, VmCowRange range);
zx_status_t rust_vm_cow_pages_unlock_range_locked(VmCowPages* cow, VmCowRange range);
__PURE uint64_t rust_vm_cow_pages_lock_order(const VmCowPages* cow);
__PURE VmCowPages* rust_vm_cow_pages_parent_locked(const VmCowPages* cow);
VmCowPages* rust_vm_cow_pages_take_parent_locked(VmCowPages* cow);
void rust_vm_cow_pages_set_parent_locked(VmCowPages* cow, VmCowPages* parent);
__PURE bool rust_vm_cow_pages_is_parent_hidden_locked(const VmCowPages* cow);
__PURE PageSource* rust_vm_cow_pages_page_source(const VmCowPages* cow);
__PURE PageSourceType rust_vm_cow_pages_page_source_type(const VmCowPages* cow);
__PURE bool rust_vm_cow_pages_can_evict(const VmCowPages* cow);
__PURE bool rust_vm_cow_pages_can_borrow(const VmCowPages* cow);
__PURE bool rust_vm_cow_pages_is_dirty_tracked(const VmCowPages* cow);
void rust_vm_cow_pages_mark_modified_locked(VmCowPages* cow);
VmCowPages* rust_vm_cow_pages_debug_get_parent(const VmCowPages* cow);
zx_koid_t rust_vm_cow_pages_get_page_source_koid(const VmCowPages* cow);
uint32_t rust_vm_cow_pages_debug_lookup_depth_locked(const VmCowPages* cow);
void rust_vm_cow_pages_release_owned_pages_range_locked(VmCowPages* cow, uint64_t offset,
                                                        uint64_t len, VmCowPages* parent,
                                                        ScopedPageFreedList* freed_list);
void rust_vm_cow_pages_invalidate_dirty_requests_locked(VmCowPages* cow, uint64_t offset,
                                                        uint64_t len);
zx_status_t rust_vm_cow_pages_commit_range_locked(VmCowPages* cow, VmCowRange range,
                                                  VmCowPages::DeferredOps* deferred,
                                                  uint64_t* committed_len,
                                                  MultiPageRequest* page_request);
zx_status_t rust_vm_cow_pages_resize(VmCowPages* cow, uint64_t s);
VmCowPagesZeroPagesResult rust_vm_cow_pages_zero_pages_locked(VmCowPages* cow, VmCowRange range,
                                                              bool dirty_track,
                                                              VmCowPages::DeferredOps* deferred,
                                                              MultiPageRequest* page_request);
}

// VmPageOrMarkerRef is passed by value to Rust (`rust_vm_cow_pages_make_page_from_reference`,
// `rust_vm_cow_pages_replace_reference_with_page_locked`,
// `rust_vm_cow_pages_begin_add_page_with_slot_locked`) as a single pointer register.
static_assert(ktl::is_trivially_copyable_v<VmPageOrMarkerRef> &&
              ktl::is_trivially_destructible_v<VmPageOrMarkerRef> &&
              sizeof(VmPageOrMarkerRef) == sizeof(VmPageOrMarker*) &&
              alignof(VmPageOrMarkerRef) == alignof(VmPageOrMarker*));

inline void VmCowPages::AssertCanary() const { rust_vm_cow_pages_assert_canary(this); }
inline uint32_t VmCowPages::pmm_alloc_flags() const {
  return rust_vm_cow_pages_pmm_alloc_flags(this);
}
inline VmCowPagesOptions VmCowPages::options() const {
  return static_cast<VmCowPagesOptions>(rust_vm_cow_pages_options(this));
}

inline uint32_t VmCowPages::children_list_len_locked() const {
  return rust_vm_cow_pages_children_list_len_locked(this);
}
inline void VmCowPages::set_children_list_len_locked(uint32_t v) {
  rust_vm_cow_pages_set_children_list_len_locked(this, v);
}
inline uint64_t VmCowPages::size_locked() const { return rust_vm_cow_pages_size_locked(this); }
inline uint64_t VmCowPages::parent_offset_locked() const {
  return rust_vm_cow_pages_parent_offset_locked(this);
}
inline void VmCowPages::set_parent_offset_locked(uint64_t v) {
  rust_vm_cow_pages_set_parent_offset_locked(this, v);
}
inline uint64_t VmCowPages::parent_limit_locked() const {
  return rust_vm_cow_pages_parent_limit_locked(this);
}
inline void VmCowPages::set_parent_limit_locked(uint64_t v) {
  rust_vm_cow_pages_set_parent_limit_locked(this, v);
}
inline uint64_t VmCowPages::root_parent_offset_locked() const {
  return rust_vm_cow_pages_root_parent_offset_locked(this);
}
inline VmPageList& VmCowPages::page_list_locked() {
  return *rust_vm_cow_pages_page_list_locked(this);
}
inline const VmPageList& VmCowPages::page_list_locked() const {
  return *rust_vm_cow_pages_page_list_locked(this);
}
inline AttributionTracker& VmCowPages::continuous_attribution_tracker_locked() {
  return *rust_vm_cow_pages_continuous_attribution_tracker_locked(this);
}
inline const AttributionTracker& VmCowPages::continuous_attribution_tracker_locked() const {
  return *rust_vm_cow_pages_continuous_attribution_tracker_locked(this);
}
inline uint64_t VmCowPages::HeapAllocationBytesLocked() const {
  return rust_vm_cow_pages_heap_allocation_bytes_locked(this);
}
inline void VmCowPages::set_root_parent_offset_locked(uint64_t v) {
  rust_vm_cow_pages_set_root_parent_offset_locked(this, v);
}
inline uint64_t VmCowPages::pinned_page_count_locked() const {
  return rust_vm_cow_pages_pinned_page_count_locked(this);
}
inline uint64_t VmCowPages::ReclamationEventCountLocked() const {
  return rust_vm_cow_pages_reclamation_event_count_locked(this);
}
inline void VmCowPages::set_reclamation_event_count_locked(uint64_t v) {
  rust_vm_cow_pages_set_reclamation_event_count_locked(this, v);
}
inline int64_t VmCowPages::high_priority_count_locked() const {
  return rust_vm_cow_pages_high_priority_count_locked(this);
}
inline void VmCowPages::set_high_priority_count_locked(int64_t v) {
  rust_vm_cow_pages_set_high_priority_count_locked(this, v);
}
inline bool VmCowPages::pager_stats_modified_locked() const {
  return rust_vm_cow_pages_pager_stats_modified_locked(this);
}
inline void VmCowPages::set_pager_stats_modified_locked(bool v) {
  rust_vm_cow_pages_set_pager_stats_modified_locked(this, v);
}
inline VmCowPages::LifeCycle VmCowPages::life_cycle_locked() const {
  return static_cast<LifeCycle>(rust_vm_cow_pages_life_cycle_locked(this));
}
inline void VmCowPages::set_life_cycle_locked(LifeCycle v) {
  rust_vm_cow_pages_set_life_cycle_locked(this, static_cast<uint8_t>(v));
}
inline bool VmCowPages::ever_pinned() const { return rust_vm_cow_pages_ever_pinned(this); }
inline bool VmCowPages::ever_pinned_locked() const {
  return rust_vm_cow_pages_ever_pinned_locked(this);
}
inline void VmCowPages::set_ever_pinned_locked(bool v) {
  rust_vm_cow_pages_set_ever_pinned_locked(this, v);
}
inline void VmCowPages::TransitionToAliveLocked() {
  rust_vm_cow_pages_transition_to_alive_locked(this);
}
inline bool VmCowPages::has_no_children_locked() const {
  return rust_vm_cow_pages_has_no_children_locked(this);
}
inline bool VmCowPages::is_high_memory_priority_locked() const {
  return rust_vm_cow_pages_is_high_memory_priority_locked(this);
}
inline void VmCowPages::ResetPagerVmoStatsLocked() {
  rust_vm_cow_pages_reset_pager_vmo_stats_locked(this);
}
inline bool VmCowPages::is_hidden() const { return rust_vm_cow_pages_is_hidden(this); }
inline bool VmCowPages::can_decommit_zero_pages() const {
  return rust_vm_cow_pages_can_decommit_zero_pages(this);
}
inline zx_status_t VmCowPages::QueryPagerVmoStatsLocked(bool reset, zx_pager_vmo_stats_t* stats) {
  return rust_vm_cow_pages_query_pager_vmo_stats_locked(this, reset, stats);
}
inline void VmCowPages::MoveToNotPinnedLocked(vm_page_t* page, uint64_t offset) {
  rust_vm_cow_pages_move_to_not_pinned_locked(this, page, offset);
}
inline void VmCowPages::SetNotPinnedLocked(vm_page_t* page, uint64_t offset) {
  rust_vm_cow_pages_set_not_pinned_locked(this, page, offset);
}
inline void VmCowPages::UpdateDirtyStateLocked(vm_page_t* page, uint64_t offset,
                                               DirtyState dirty_state, bool is_pending_add) {
  rust_vm_cow_pages_update_dirty_state_locked(this, page, offset, dirty_state, is_pending_add);
}
inline void VmCowPages::FinishCachePolicyTransitionLocked() {
  rust_vm_cow_pages_finish_cache_policy_transition_locked(this);
}
inline bool VmCowPages::AnyPagesPinnedLocked(uint64_t offset, size_t len) {
  return rust_vm_cow_pages_any_pages_pinned_locked(this, offset, len);
}
inline bool VmCowPages::DebugIsRangePinnedLocked(VmCowRange range) {
  return rust_vm_cow_pages_debug_is_range_pinned_locked(this, range);
}
inline zx_status_t VmCowPages::LookupLocked(VmCowRange range, VmObject::LookupFunction lookup_fn) {
  return rust_vm_cow_pages_lookup_locked(this, range, &lookup_fn);
}
inline zx_status_t VmCowPages::EnumerateDirtyRangesLocked(
    VmCowRange range, DirtyRangeEnumerateFunction&& dirty_range_fn) {
  return rust_vm_cow_pages_enumerate_dirty_ranges_locked(this, range, &dirty_range_fn);
}
inline bool VmCowPages::DebugValidateContinuousAttribution() const {
  return rust_vm_cow_pages_debug_validate_continuous_attribution_locked(this);
}
inline uint64_t VmCowPages::DebugGetPageCountLocked() const {
  return rust_vm_cow_pages_debug_get_page_count_locked(this);
}
inline bool VmCowPages::DebugIsPage(uint64_t offset) const {
  return rust_vm_cow_pages_debug_is_page(this, offset);
}
inline bool VmCowPages::DebugIsMarker(uint64_t offset) const {
  return rust_vm_cow_pages_debug_is_marker(this, offset);
}
inline uint32_t VmCowPages::DebugGetMarkerShareCount(uint64_t offset) const {
  return rust_vm_cow_pages_debug_get_marker_share_count(this, offset);
}
inline bool VmCowPages::DebugIsParentContent(uint64_t offset) const {
  return rust_vm_cow_pages_debug_is_parent_content(this, offset);
}
inline bool VmCowPages::DebugIsEmpty(uint64_t offset) const {
  return rust_vm_cow_pages_debug_is_empty(this, offset);
}
inline vm_page_t* VmCowPages::DebugGetPage(uint64_t offset) const {
  return rust_vm_cow_pages_debug_get_page(this, offset);
}
inline vm_page_t* VmCowPages::DebugGetPageLocked(uint64_t offset) const {
  return rust_vm_cow_pages_debug_get_page_locked(this, offset);
}
inline bool VmCowPages::DebugIsHighMemoryPriority() const {
  return rust_vm_cow_pages_debug_is_high_memory_priority(this);
}
inline uint32_t VmCowPages::DebugGetPopulatedSlotsCount() const {
  return rust_vm_cow_pages_debug_get_populated_slots_count(this);
}
inline PmmOptDelayReuse VmCowPages::should_delay_reuse_on_free() const {
  return rust_vm_cow_pages_should_delay_reuse_on_free(this);
}
inline void VmCowPages::FreePages(VmPageDoublyLinkedList* pages) {
  rust_vm_cow_pages_free_pages(this, pages);
}
inline void VmCowPages::FreePage(vm_page_t* page) { rust_vm_cow_pages_free_page(this, page); }
inline zx_status_t VmCowPages::AllocUninitializedPage(vm_page_t** page,
                                                      AnonymousPageRequest* page_request) {
  return rust_vm_cow_pages_alloc_uninitialized_page(this, page, page_request);
}
inline zx_status_t VmCowPages::AllocPage(vm_page_t** page, AnonymousPageRequest* page_request) {
  return rust_vm_cow_pages_alloc_page(this, page, page_request);
}
inline zx_status_t VmCowPages::MakePageFromReference(VmPageOrMarkerRef page_or_mark,
                                                     AnonymousPageRequest* page_request) {
  return rust_vm_cow_pages_make_page_from_reference(this, page_or_mark, page_request);
}
inline zx_status_t VmCowPages::ReplaceReferenceWithPageLocked(VmPageOrMarkerRef page_or_mark,
                                                              uint64_t offset,
                                                              AnonymousPageRequest* page_request) {
  return rust_vm_cow_pages_replace_reference_with_page_locked(this, page_or_mark, offset,
                                                              page_request);
}
inline VmCowPages::DeferredOps::DeferredOps(VmCowPages* self) {
  DEBUG_ASSERT(self);
  rust_vm_cow_pages_deferred_ops_init(this, self);
}
inline VmCowPages::DeferredOps::~DeferredOps() { rust_vm_cow_pages_deferred_ops_destroy(this); }
inline ScopedPageFreedList& VmCowPages::DeferredOps::FreedList(VmCowPages* self) {
  return *rust_vm_cow_pages_deferred_ops_freed_list(this, self);
}
inline const VmCowPages* VmCowPages::DeferredOps::self() const {
  return rust_vm_cow_pages_deferred_ops_self(this);
}
inline void VmCowPages::RemovePageLocked(vm_page_t* page, DeferredOps& ops) {
  rust_vm_cow_pages_remove_page_locked(this, page, &ops);
}
inline void VmCowPages::RangeChangeUpdateLocked(VmCowRange range, RangeChangeOp op,
                                                DeferredOps* deferred) {
  rust_vm_cow_pages_range_change_update_locked(this, range, op, deferred);
}
inline void VmCowPages::RangeChangeUpdateMappingsLocked(VmObjectPaged& paged, VmCowRange range,
                                                        RangeChangeOp op) {
  rust_vm_cow_pages_range_change_update_mappings_locked(this, &paged, range, op);
}
inline VmCowPages::AddPageTransaction::AddPageTransaction(AddPageTransaction&& other) {
  rust_vm_cow_pages_add_page_transaction_move(this, &other);
}
inline VmCowPages::AddPageTransaction::~AddPageTransaction() {
  rust_vm_cow_pages_add_page_transaction_destroy(this);
}
inline VmCowPages::LookupCursor::LookupCursor(VmCowPages* target, VmCowRange range) {
  rust_vm_cow_pages_lookup_cursor_init(this, target, range.offset, range.len);
}
inline VmCowPages::LookupCursor::~LookupCursor() { rust_vm_cow_pages_lookup_cursor_destroy(this); }
inline zx::result<VmCowPages::LookupCursor::RequireResult>
VmCowPages::LookupCursor::RequireOwnedPage(bool will_write, uint64_t max_request_pages,
                                           DeferredOps& deferred, MultiPageRequest* page_request) {
  DEBUG_ASSERT(page_request);
  const VmCowPagesLookupCursorRequireResult result =
      rust_vm_cow_pages_lookup_cursor_require_owned_page(this, will_write, max_request_pages,
                                                         &deferred, page_request);
  if (result.status != ZX_OK) {
    return zx::error(result.status);
  }
  return zx::ok(RequireResult{reinterpret_cast<vm_page_t*>(result.page), result.writable});
}
inline zx::result<VmCowPages::LookupCursor::RequireResult> VmCowPages::LookupCursor::RequirePage(
    bool will_write, uint64_t max_request_pages, DeferredOps& deferred,
    MultiPageRequest* page_request) {
  DEBUG_ASSERT(page_request);
  const VmCowPagesLookupCursorRequireResult result = rust_vm_cow_pages_lookup_cursor_require_page(
      this, will_write, max_request_pages, &deferred, page_request);
  if (result.status != ZX_OK) {
    return zx::error(result.status);
  }
  return zx::ok(RequireResult{reinterpret_cast<vm_page_t*>(result.page), result.writable});
}
inline vm_page_t* VmCowPages::LookupCursor::MaybePage(bool will_write) {
  return rust_vm_cow_pages_lookup_cursor_maybe_page(this, will_write);
}
inline void VmCowPages::LookupCursor::DisableZeroFork() {
  rust_vm_cow_pages_lookup_cursor_disable_zero_fork(this);
}
inline void VmCowPages::LookupCursor::DisableMarkAccessed() {
  rust_vm_cow_pages_lookup_cursor_disable_mark_accessed(this);
}
inline VmCowPages* VmCowPages::LookupCursor::target() const {
  return rust_vm_cow_pages_lookup_cursor_target(this);
}
inline zx::result<VmCowPages::AddPageTransaction> VmCowPages::BeginAddPageWithSlotLocked(
    uint64_t offset, VmPageOrMarkerRef slot, CanOverwriteSlot overwrite) {
  __UNINITIALIZED AddPageTransaction transaction;
  zx_status_t status = rust_vm_cow_pages_begin_add_page_with_slot_locked(this, offset, slot,
                                                                         overwrite, &transaction);
  if (unlikely(status != ZX_OK)) {
    return zx::error(status);
  }
  return zx::ok(ktl::move(transaction));
}
inline zx::result<VmCowPages::AddPageTransaction> VmCowPages::BeginAddPageLocked(
    uint64_t offset, CanOverwriteSlot overwrite) {
  __UNINITIALIZED AddPageTransaction transaction;
  zx_status_t status =
      rust_vm_cow_pages_begin_add_page_locked(this, offset, overwrite, &transaction);
  if (unlikely(status != ZX_OK)) {
    return zx::error(status);
  }
  return zx::ok(ktl::move(transaction));
}
inline VmPageOrMarker VmCowPages::CompleteAddPageLocked(AddPageTransaction& transaction,
                                                        VmPageOrMarker&& p, DeferredOps* deferred) {
  VmPageOrMarker old = VmPageOrMarker::Empty();
  rust_vm_cow_pages_complete_add_page_locked(this, &transaction, &p, deferred, &old);
  return old;
}
inline zx_status_t VmCowPages::AddNewPageLocked(uint64_t offset, vm_page_t* page,
                                                CanOverwriteSlot overwrite,
                                                VmPageOrMarker* released_page, bool zero,
                                                DeferredOps* deferred) {
  return rust_vm_cow_pages_add_new_page_locked(this, offset, page, overwrite, released_page, zero,
                                               deferred);
}
inline zx_status_t VmCowPages::AddNewPagesLocked(uint64_t start_offset,
                                                 VmPageDoublyLinkedList* pages,
                                                 CanOverwriteSlot overwrite, bool zero,
                                                 DeferredOps* deferred) {
  return rust_vm_cow_pages_add_new_pages_locked(this, start_offset, pages, overwrite, zero,
                                                deferred);
}
inline zx_status_t VmCowPages::DecommitRange(VmCowRange range) {
  return rust_vm_cow_pages_decommit_range(this, range);
}
inline VmCowPages::AttributionCounts VmCowPages::GetAttributedMemoryInRangeLocked(
    VmCowRange range) const {
  AttributionCounts counts;
  rust_vm_cow_pages_get_attributed_memory_in_range_locked(this, range, &counts);
  return counts;
}
inline bool VmCowPages::PageWouldReadZeroLocked(uint64_t page_offset) {
  return rust_vm_cow_pages_page_would_read_zero_locked(this, page_offset) != 0;
}
inline zx_status_t VmCowPages::LookupReadableLocked(VmCowRange range,
                                                    LookupReadableFunction lookup_fn) {
  return rust_vm_cow_pages_lookup_readable_locked(this, range, &lookup_fn);
}
inline zx_status_t VmCowPages::PrepareForWriteLocked(VmCowRange range,
                                                     LazyPageRequest* page_request,
                                                     uint64_t* dirty_len_out) {
  return rust_vm_cow_pages_prepare_for_write_locked(this, range, page_request, dirty_len_out);
}
inline zx_status_t VmCowPages::FailPageRequestsLocked(VmCowRange range, zx_status_t error_status) {
  return rust_vm_cow_pages_fail_page_requests_locked(this, range, error_status);
}
inline zx_status_t VmCowPages::WritebackBeginLocked(VmCowRange range, bool is_zero_range) {
  return rust_vm_cow_pages_writeback_begin_locked(this, range, is_zero_range);
}
inline zx_status_t VmCowPages::WritebackEndLocked(VmCowRange range) {
  return rust_vm_cow_pages_writeback_end_locked(this, range);
}
inline void VmCowPages::DetachSource() { rust_vm_cow_pages_detach_source(this); }
inline zx::result<uint64_t> VmCowPages::UnmapAndFreePagesLocked(uint64_t offset, uint64_t len,
                                                                DeferredOps& deferred) {
  uint64_t freed_count = 0;
  zx_status_t status =
      rust_vm_cow_pages_unmap_and_free_pages_locked(this, offset, len, &deferred, &freed_count);
  if (unlikely(status != ZX_OK)) {
    return zx::error(status);
  }
  return zx::ok(freed_count);
}
inline zx_status_t VmCowPages::PinRangeLocked(VmCowRange range) {
  return rust_vm_cow_pages_pin_range_locked(this, range);
}
inline void VmCowPages::UnpinLocked(VmCowRange range, DeferredOps* deferred) {
  rust_vm_cow_pages_unpin_locked(this, range, deferred);
}
inline zx_status_t VmCowPages::LockRangeLocked(VmCowRange range,
                                               zx_vmo_lock_state_t* lock_state_out) {
  return rust_vm_cow_pages_lock_range_locked(this, range, lock_state_out);
}
inline zx_status_t VmCowPages::TryLockRangeLocked(VmCowRange range) {
  return rust_vm_cow_pages_try_lock_range_locked(this, range);
}
inline zx_status_t VmCowPages::UnlockRangeLocked(VmCowRange range) {
  return rust_vm_cow_pages_unlock_range_locked(this, range);
}
inline uint64_t VmCowPages::lock_order() const { return rust_vm_cow_pages_lock_order(this); }
inline VmCowPages* VmCowPages::parent_locked() const {
  return rust_vm_cow_pages_parent_locked(this);
}
inline fbl::RefPtr<VmCowPages> VmCowPages::take_parent_locked() {
  return fbl::ImportFromRawPtr(rust_vm_cow_pages_take_parent_locked(this));
}
inline void VmCowPages::set_parent_locked(fbl::RefPtr<VmCowPages> parent) {
  rust_vm_cow_pages_set_parent_locked(this, fbl::ExportToRawPtr(&parent));
}
inline bool VmCowPages::is_parent_hidden_locked() const {
  return rust_vm_cow_pages_is_parent_hidden_locked(this);
}
inline PageSource* VmCowPages::page_source() const { return rust_vm_cow_pages_page_source(this); }
inline PageSourceType VmCowPages::page_source_type() const {
  return rust_vm_cow_pages_page_source_type(this);
}
inline bool VmCowPages::can_evict() const { return rust_vm_cow_pages_can_evict(this); }
inline bool VmCowPages::can_borrow() const { return rust_vm_cow_pages_can_borrow(this); }
inline bool VmCowPages::is_dirty_tracked() const {
  return rust_vm_cow_pages_is_dirty_tracked(this);
}
inline void VmCowPages::mark_modified_locked() { rust_vm_cow_pages_mark_modified_locked(this); }
inline fbl::RefPtr<VmCowPages> VmCowPages::DebugGetParent() const {
  return fbl::ImportFromRawPtr(rust_vm_cow_pages_debug_get_parent(this));
}
inline ktl::optional<zx_koid_t> VmCowPages::GetPageSourceKoid() const {
  const zx_koid_t koid = rust_vm_cow_pages_get_page_source_koid(this);
  if (koid == ZX_KOID_INVALID) {
    return ktl::nullopt;
  }
  return koid;
}
inline uint32_t VmCowPages::DebugLookupDepthLocked() const {
  return rust_vm_cow_pages_debug_lookup_depth_locked(this);
}
inline void VmCowPages::ReleaseOwnedPagesRangeLocked(uint64_t offset, uint64_t len,
                                                     const LockedPtr& parent,
                                                     ScopedPageFreedList& freed_list) {
  rust_vm_cow_pages_release_owned_pages_range_locked(this, offset, len, parent.get(), &freed_list);
}
inline void VmCowPages::InvalidateDirtyRequestsLocked(uint64_t offset, uint64_t len) {
  rust_vm_cow_pages_invalidate_dirty_requests_locked(this, offset, len);
}
inline zx_status_t VmCowPages::CommitRangeLocked(VmCowRange range, DeferredOps& deferred,
                                                 uint64_t* committed_len,
                                                 MultiPageRequest& page_request) {
  return rust_vm_cow_pages_commit_range_locked(this, range, &deferred, committed_len,
                                               &page_request);
}
inline zx_status_t VmCowPages::Resize(uint64_t size) {
  return rust_vm_cow_pages_resize(this, size);
}
inline ktl::pair<zx_status_t, uint64_t> VmCowPages::ZeroPagesLocked(
    VmCowRange range, bool dirty_track, DeferredOps& deferred, MultiPageRequest& page_request) {
  const VmCowPagesZeroPagesResult result =
      rust_vm_cow_pages_zero_pages_locked(this, range, dirty_track, &deferred, &page_request);
  return {result.status, result.zeroed_len};
}

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_VM_COW_PAGES_H_
