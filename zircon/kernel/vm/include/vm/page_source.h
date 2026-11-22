// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_SOURCE_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_SOURCE_H_

#include <lib/zx/result.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <fbl/recycler.h>
#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>
#include <kernel/event.h>
#include <kernel/ffi.h>
#include <kernel/mutex.h>
#include <ktl/optional.h>
#include <vm/anonymous_page_request.h>
#include <vm/page.h>
#include <vm/vm_constants.h>

// At the high level the goal of the objects here is to
// 1. Trigger external entities to do work based on VMO operations, such as asking a pager to supply
//    a missing page of data.
// 2. Have a way for external entities to let the VMO system know these requests have been
//    fulfilled.
// 3. Provide a way for the high level caller, who may not know what actions are being performed on
//    what entities, to wait until their operation can be completed.
//
// The different objects can be summarized as:
//  * PageRequest: Caller allocated object that the caller uses to perform the Wait.
//  * PageSource: Performs request and overlap tracking, forwarding unique ranges of requests to the
//    underlying PageProvider.
//  * PageProvider: Asynchronously performs requests. Requests are completed by actions being
//    performed on the VMO.
//
// A typical flow would be
//  * User allocates PageRequest on the stack, and passes it in to some VMO operation
//  * VMO code needs something to happen and calls a PageSource method, passing in PageRequest it
//    had been given.
//  * PageSource populates fields of the PageRequest and adds it to the list of requests it is
//    tracking, and determines how this request overlaps with any others. Based on overlap, it may
//    or may not notify the underlying PageProvider that some work needs to be done (the page
//    provider will complete this asynchronously somehow).
//  * VMO returns ZX_ERR_SHOULD_WAIT and then the top level calls PageRequest::Wait
//  * PageRequest::Wait asks the underlying PageSource how to Wait for the operation to complete
//  # As an optional path, if the PageRequest was not Waited on for some reason, the PageRequest
//    will also inform the PageSource that this request is no longer needed and can be canceled.
// For the other side, while the Wait is happening some other thread will
//  * Call a VMO operation, such as VmObject::SupplyPages
//  * VMO will perform the operation, and then let the PageSource know by the corresponding
//    interface method, such as OnPagesSupplied.
//  * PageSource will update request tracking, and notify any PageRequests that were waiting and can
//    be woken up.
//
// There is more complexity of implementation and API, largely to handle the fact that the
// PageRequest serves as the allocation of all data needed for all parties. Therefore every layer
// needs to be told when requests are coming and going to ensure they update any lists and do not
// refer to out of scope stack variables.
//
// PageSource, PageRequest, LazyPageRequest and MultiPageRequest are implemented in Rust (see
// page_source.rs). The C++ classes below are shells that forward to the Rust implementation.

class PageRequest;
class PageSource;
class PageProvider;
class LazyPageRequest;
class MultiPageRequest;

struct VmoDebugInfo {
  uint64_t vmo_id;
  char vmo_name[8];
};

// The different types of page requests that can exist.
enum page_request_type : uint32_t {
  READ = 0,   // Request to provide the initial contents for the page.
  DIRTY,      // Request to alter contents of the page, i.e. transition it from clean to dirty.
  WRITEBACK,  // Request to write back modified page contents back to the source.
  COUNT       // Number of page request types.
};

// These properties are constant per PageProvider type, so a given VmCowPages can query and cache
// these properties once (if it has a PageSource) and know they won't change after that.  This also
// avoids per-property plumbing via PageSource.
//
// TODO(dustingreen): (or rashaeqbal) Migrate more const per-PageProvider-type properties to
// PageSourceProperties, after the initial round of merging is done.
struct PageSourceProperties {
  // We use PageSource for both user pager and contiguous page reclaim.  This is how we tell whether
  // the PageSource is really a user pager when reporting to user mode that a given VMO is/isn't
  // user pager backed.
  //
  // If this is true, the VmCowPages may include both immutable page content and page content that
  // was potentially modified and written back previously.
  //
  // If this is false, the PageSource (and PageProvider) must be used to allocate all pages.
  // Pre-allocating generic pages from the pmm won't work. These pages must be specifically returned
  // via PageSource::FreePages instead of pmm_free.
  const bool is_user_pager;

  // For every entry, if true the PageSource supports the given |page_request_type|.
  const bool supports_request_type[page_request_type::COUNT];
};

extern "C" {
// Rust PageSource methods exposed to C++. See page_source.rs.
zx_status_t rust_page_source_create(PageProvider* page_provider, PageSource** out);
void rust_page_source_recycle(PageSource* src);
void rust_page_source_on_pages_supplied(PageSource* src, uint64_t offset, uint64_t len);
void rust_page_source_on_pages_failed(PageSource* src, uint64_t offset, uint64_t len,
                                      zx_status_t error_status);
bool rust_page_source_is_valid_internal_failure_code(zx_status_t error_status) __PURE;
bool rust_page_source_should_trap_dirty_transitions(const PageSource* src) __PURE;
zx_status_t rust_page_source_request_dirty_transition(PageSource* src, PageRequest* request,
                                                      uint64_t offset, uint64_t len,
                                                      VmoDebugInfo vmo_debug_info);
void rust_page_source_on_pages_dirtied(PageSource* src, uint64_t offset, uint64_t len);
void rust_page_source_detach(PageSource* src);
void rust_page_source_close(PageSource* src);
PageSourceProperties rust_page_source_properties(const PageSource* src) __PURE;
void rust_page_source_dump(const PageSource* src, uint depth, uint32_t max_items);
bool rust_page_source_is_detached(const PageSource* src);
Lock<Mutex>* rust_page_source_paged_vmo_lock(PageSource* src);
bool rust_page_source_get_provider_koid(const PageSource* src, zx_koid_t* out_koid);

// Rust LazyPageRequest and MultiPageRequest methods exposed to C++.
PageRequest* rust_lazy_page_request_get(LazyPageRequest* req);
void rust_multi_page_request_init(MultiPageRequest* req, bool early_wake);
void rust_multi_page_request_destroy(MultiPageRequest* req);
zx_status_t rust_multi_page_request_wait(MultiPageRequest* req, bool suspendable);
AnonymousPageRequest* rust_multi_page_request_get_anonymous(MultiPageRequest* req);
LazyPageRequest* rust_multi_page_request_get_lazy_dirty_request(MultiPageRequest* req);
void rust_multi_page_request_made_dirty_request(MultiPageRequest* req);
void rust_multi_page_request_cancel_requests(MultiPageRequest* req);
}

// Interface for providing pages to a VMO through page requests.
class PageProvider : public fbl::RefCounted<PageProvider> {
 public:
  virtual ~PageProvider() = default;

 private:
  // The returned properties can assumed to be const and never change. As such the caller may cache
  // them.
  virtual PageSourceProperties properties() const = 0;

  // Informs the backing source of a page request. The provider has ownership
  // of |request| until the async request is cancelled.
  virtual void SendAsyncRequest(PageRequest* request) = 0;
  // Informs the backing source that a page request has been fulfilled. This
  // must be called for all requests that are raised.
  virtual void ClearAsyncRequest(PageRequest* request) = 0;
  // Swaps the backing memory for a request. Assumes that |old|
  // and |new_request| have the same type, offset, and length.
  virtual void SwapAsyncRequest(PageRequest* old, PageRequest* new_req) = 0;
  // Frees |pages|.
  // This will assert unless is_handling_free is true, in which case this will make the pages FREE.
  virtual void FreePages(VmPageDoublyLinkedList* pages) {
    // If is_handling_free true, must implement FreePages().
    ASSERT(false);
  }
  // For asserting purposes only.  This gives the PageProvider a chance to check that a page is
  // consistent with any rules the PageProvider has re. which pages can go where in the VmCowPages.
  // PhysicalPageProvider implements this to verify that page at offset makes sense with respect to
  // phys_base_, since VmCowPages can't do that on its own due to lack of knowledge of phys_base_
  // and lack of awareness of contiguous.
  virtual bool DebugIsPageOk(vm_page_t* page, uint64_t offset) = 0;

  // OnDetach is called once no more calls to SendAsyncRequest will be made. It will be called
  // before OnClose and will only be called once.
  virtual void OnDetach() = 0;
  // After OnClose is called, no more calls will be made except for ::WaitOnEvent.
  virtual void OnClose() = 0;

  // Waits on an |event| associated with a page request. The waiting thread can return early from
  // the wait due to a suspend signal only if |suspendable| is true.
  virtual zx_status_t WaitOnEvent(Event* event, bool suspendable) = 0;

  // Dumps relevant state for debugging purposes. The |max_items| parameter should be used to cap
  // the number of elements printed from any kind of variable sized list to prevent spam.
  virtual void Dump(uint depth, uint max_items) = 0;

  // Retrieves the koid for this page provider, if it has one.
  virtual ktl::optional<uint64_t> GetKoid() const { return ktl::nullopt; }

  // Retrieves the debug name for this page provider, if it has one.
  virtual void GetDebugName(char (&name)[ZX_MAX_NAME_LEN]) const { name[0] = '\0'; }

  // Rust reaches the private virtual interface above through the cpp_page_provider_* shims in
  // page_source_ffi.cc, which PageProviderFfi forwards on their behalf.
  friend struct PageProviderFfi;
};

// A page source is responsible for fulfilling page requests from a VMO with backing pages.
// The PageSource class mostly contains generic functionality around managing
// the lifecycle of VMO page requests. The PageSource contains an reference to a PageProvider
// implementation, which is responsible for actually providing the pages. (E.g. for VMOs backed by a
// userspace pager, the PageProvider is a PagerProxy instance which talks to the userspace pager
// service.)
//
// The synchronous fulfillment of requests is fairly straightforward, with direct calls
// from the vm object to the PageSource to the PageProvider.
//
// For asynchronous requests, the lifecycle is as follows:
//   1) A vm object requests a page with PageSource::GetPage.
//   2) PageSource starts tracking the request's PageRequest and then
//      forwards the request to PageProvider::SendAsyncRequest.
//   3) The caller waits for the request with PageRequest::Wait.
//   4) At some point, whatever is backing the PageProvider provides pages
//      to the vm object (e.g. with VmObjectPaged::SupplyPages).
//   5) The vm object calls PageSource::OnPagesSupplied, which signals
//      any PageRequests that have been fulfilled.
//   6) The caller wakes up and queries the vm object again, by which
//      point the requested page will be present.
//
// For a contiguous VMO requesting physical pages back, step 4 above just frees the pages from some
// other use, and step 6 finds the requested pages available, but not yet present in the VMO,
// similar to what can happen with a normal PageProvider where pages can be read and then
// decommitted before the caller queries the vm object again.

// Object which provides pages to a vm_object.
class PageSource final : public fbl::RefCounted<PageSource>, public fbl::Recyclable<PageSource> {
 public:
  PageSource() = delete;

  // Creates a PageSource that forwards page requests to |page_provider|.
  static zx::result<fbl::RefPtr<PageSource>> Create(fbl::RefPtr<PageProvider>&& page_provider) {
    PageSource* ptr = nullptr;
    zx_status_t status = rust_page_source_create(fbl::ExportToRawPtr(&page_provider), &ptr);
    if (status != ZX_OK) {
      return zx::error(status);
    }
    return zx::ok(fbl::ImportFromRawPtr(ptr));
  }

  // Updates the request tracking metadata to account for pages [offset, offset + len) having
  // been supplied to the owning vmo.
  //
  // Note that the range [offset, offset + len) should not have been previously supplied. The page
  // request tracking in PageSource works by tracking only a fulfilled length, and not exact
  // fulfilled offsets, to save on memory required for metadata. So in order to prevent
  // over-accounting errors, the caller must ensure that they are only calling OnPagesSupplied for
  // newly supplied ranges.
  // TODO(rashaeqbal): Consider relaxing this constraint by more precise tracking of fulfilled
  // offsets with a bitmap. Might require capping the max permissible length of a page request.
  void OnPagesSupplied(uint64_t offset, uint64_t len) {
    rust_page_source_on_pages_supplied(this, offset, len);
  }

  // Fails outstanding page requests in the range [offset, offset + len). Events associated with the
  // failed page requests are signaled with the |error_status|, and any waiting threads are
  // unblocked.
  void OnPagesFailed(uint64_t offset, uint64_t len, zx_status_t error_status) {
    rust_page_source_on_pages_failed(this, offset, len, error_status);
  }

  // Returns true if |error_status| is a valid provider failure error code, which can be used with
  // |OnPagesFailed|.
  //
  // This returns true for every error code that PageSource::is_valid_external_failure_code (in
  // page_source.rs) returns true for, plus any additional error codes that are valid as an
  // internal PageProvider status but not valid for ZX_PAGER_OP_FAIL.
  //
  // ZX_ERR_NO_MEMORY will return true, unlike
  // PageSource::is_valid_external_failure_code(Status::NO_MEMORY) which returns false.
  //
  // Not every error code is supported, since these errors can get returned via a zx_vmo_read() or a
  // zx_vmo_op_range(), if those calls resulted in a page fault.  So the |error_status| should be a
  // supported return error code for those syscalls.  An error code need not be specifiable via
  // ZX_PAGER_OP_FAIL for this function to return true.
  static bool IsValidInternalFailureCode(zx_status_t error_status) {
    return rust_page_source_is_valid_internal_failure_code(error_status);
  }

  // Whether transitions from clean to dirty should be trapped.
  bool ShouldTrapDirtyTransitions() const {
    return rust_page_source_should_trap_dirty_transitions(this);
  }

  // Request the page provider for clean pages in the range [offset, offset + len) to become dirty,
  // in order for a write to proceed. Returns ZX_ERR_SHOULD_WAIT if the request will be
  // asynchronously fulfilled; the caller should wait on |request|. Depending on the state of pages
  // in the range, the |request| might be generated for a range that is a subset of
  // [offset, offset + len).
  zx_status_t RequestDirtyTransition(PageRequest* request, uint64_t offset, uint64_t len,
                                     VmoDebugInfo vmo_debug_info) {
    return rust_page_source_request_dirty_transition(this, request, offset, len, vmo_debug_info);
  }

  // Updates the request tracking metadata to account for pages [offset, offset + len) having
  // been dirtied in the owning VMO.
  //
  // Note that the range [offset, offset + len) should not have been previously dirtied. The page
  // request tracking in PageSource works by tracking only a fulfilled length, and not exact
  // fulfilled offsets, to save on memory required for metadata. So in order to prevent
  // over-accounting errors, the caller must ensure that they are only calling OnPagesDirtied for
  // newly dirtied ranges.
  // TODO(rashaeqbal): Consider relaxing this constraint by more precise tracking of fulfilled
  // offsets with a bitmap. Might require capping the max permissible length of a page request.
  void OnPagesDirtied(uint64_t offset, uint64_t len) {
    rust_page_source_on_pages_dirtied(this, offset, len);
  }

  // Detaches the source from the VMO. All future calls into the page source will fail. All
  // pending read transactions are aborted. Pending flush transactions will still
  // be serviced.
  void Detach() { rust_page_source_detach(this); }

  // Closes the source. Will call Detach() if the source is not already detached. All pending
  // transactions will be aborted and all future calls will fail.
  void Close() { rust_page_source_close(this); }

  // The returned properties can assumed to be const and never change. As such the caller may cache
  // them.
  PageSourceProperties properties() const { return rust_page_source_properties(this); }

  // Prints state of the page source and any pending requests. The maximum number of requests
  // printed is capped by |max_items|.
  void Dump(uint depth, uint32_t max_items) const { rust_page_source_dump(this, depth, max_items); }

  bool is_detached() const { return rust_page_source_is_detached(this); }

  // Method for the VmCowPages to retrieve the lock for paged VMOs.
  // See VmCowPages::DeferredOps.
  Lock<Mutex>* paged_vmo_lock() { return rust_page_source_paged_vmo_lock(this); }

  // Retrieves any koid for the linked page provider.
  ktl::optional<zx_koid_t> GetProviderKoid() const {
    zx_koid_t koid;
    if (!rust_page_source_get_provider_koid(this, &koid)) {
      return ktl::nullopt;
    }
    return koid;
  }

  // The object is allocated and freed by Rust once the last reference is dropped.
  void fbl_recycle() { rust_page_source_recycle(this); }
};

// Wrapper around PageRequest that performs construction on first access. This is useful when a
// PageRequest needs to be allocated eagerly in case it is used, even if the common case is that it
// will not be needed.
class LazyPageRequest {
 public:
  LazyPageRequest() = delete;
  ~LazyPageRequest() = delete;
  DISALLOW_COPY_ASSIGN_AND_MOVE(LazyPageRequest);

  // Initialize and return the internal PageRequest.
  PageRequest* get() { return rust_lazy_page_request_get(this); }
};

// Wrapper around tracking multiple different page requests that might need waiting. Only one
// individual request is allowed to considered 'active' at a time as the one that next needs waiting
// on. Tracking whether a request is active is, depending on the request type, partially automatic
// and partially requiring additional input from the user.
// The PageRequest and LazyPageRequest access methods do not currently have a way to enforce that
// those specific types of requests are made with the returned objects, however this could change
// and callers are expected to use the correct method.
// TODO(adanis): Implement an enforcement strategy.
class MultiPageRequest {
 public:
  MultiPageRequest() : MultiPageRequest(false) {}
  explicit MultiPageRequest(bool early_wake) { rust_multi_page_request_init(this, early_wake); }
  ~MultiPageRequest() { rust_multi_page_request_destroy(this); }

  // Wait on the currently active page request. The waiting thread is suspendable by default.
  zx_status_t Wait(bool suspendable = true) {
    return rust_multi_page_request_wait(this, suspendable);
  }

  // Retrieve the anonymous page request. The caller may or may not arm the AnonymousPageRequest, if
  // it does the anonymous request becomes considered active and no other request may be retrieved.
  AnonymousPageRequest* GetAnonymous() { return rust_multi_page_request_get_anonymous(this); }

  // Retrieve a lazy accessor to page request. If a dirty request is generated the caller must
  // then call MadeDirtyRequest so that this helper knows that the page request is active and
  // should be waited on.
  LazyPageRequest* GetLazyDirtyRequest() {
    return rust_multi_page_request_get_lazy_dirty_request(this);
  }

  // Indicate that the page request retrieved by GetLazyDirtyRequest was used and should be waited
  // on.
  void MadeDirtyRequest() { rust_multi_page_request_made_dirty_request(this); }

  // Cancel all requests and have no active request.
  void CancelRequests() { rust_multi_page_request_cancel_requests(this); }

  DISALLOW_COPY_ASSIGN_AND_MOVE(MultiPageRequest);

 private:
  // Storage for the Rust MultiPageRequest. The Rust functions are passed |this|, which is the
  // address of this (only) member.
  [[maybe_unused]] OpaqueStorage<kMultiPageRequestSize, kMultiPageRequestAlign> storage_;
};

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_SOURCE_H_
