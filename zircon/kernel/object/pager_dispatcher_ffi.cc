// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/errors.h>
#include <zircon/types.h>

#include <fbl/alloc_checker.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/utility.h>
#include <object/handle.h>
#include <object/pager_dispatcher.h>
#include <object/pager_proxy.h>
#include <object/port_dispatcher.h>
#include <vm/page_source.h>

extern "C" {

zx_status_t cpp_pager_dispatcher_create(
    ffi::Uninitialized<KernelHandle<PagerDispatcher>>* handle_out) {
  fbl::AllocChecker ac;
  KernelHandle new_handle(fbl::AdoptRef(new (&ac) PagerDispatcher()));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  handle_out->Initialize(ktl::move(new_handle));
  return ZX_OK;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_pager_proxy_free(PagerProxy* proxy) { delete proxy; }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void* cpp_pager_proxy_get_ref_counted(PagerProxy* proxy) {
  return static_cast<fbl::RefCounted<PageProvider>*>(proxy);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const void* cpp_pager_proxy_get_dll_node(const PagerProxy* proxy) {
  using Traits = fbl::DefaultDoublyLinkedListTraits<fbl::RefPtr<PagerProxy>, fbl::DefaultObjectTag>;
  using NodeState = Traits::NodeState;
  static_assert(sizeof(NodeState) == 2 * sizeof(void*));
  static_assert(alignof(NodeState) == alignof(void*));
  return &Traits::node_state(*const_cast<PagerProxy*>(proxy));
}

zx_status_t cpp_pager_proxy_create(PagerDispatcher* dispatcher, PortDispatcher* port, uint64_t key,
                                   uint32_t options, PagerProxy** out_proxy) {
  fbl::AllocChecker ac;
  auto proxy = fbl::MakeRefCountedChecked<PagerProxy>(&ac, dispatcher, fbl::ImportFromRawPtr(port),
                                                      key, options);
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  *out_proxy = fbl::ExportToRawPtr(&proxy);
  return ZX_OK;
}

zx_status_t cpp_pager_proxy_create_page_source(PagerProxy* proxy, PageSource** out_src) {
  fbl::AllocChecker ac;
  auto src = fbl::MakeRefCountedChecked<PageSource>(&ac, fbl::RefPtr<PageProvider>(proxy));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  *out_src = fbl::ExportToRawPtr(&src);
  return ZX_OK;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_pager_proxy_set_page_source_unchecked(PagerProxy* proxy,
                                                                 PageSource* src) {
  proxy->SetPageSourceUnchecked(fbl::ImportFromRawPtr(src));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_pager_proxy_on_dispatcher_close(PagerProxy* proxy) {
  proxy->OnDispatcherClose();
}

}  // extern "C"
