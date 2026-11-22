// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PAGER_PROXY_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PAGER_PROXY_H_

#include <lib/object-constants.h>
#include <zircon/types.h>

#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <object/port_dispatcher.h>
#include <vm/page_source.h>

class PagerDispatcher;

extern "C" {
// Rust PagerProxyState methods exposed to the C++ PagerProxy shell. See pager_proxy.rs.
void rust_pager_proxy_state_init(void* state, PagerDispatcher* dispatcher, PortDispatcher* port,
                                 uint64_t key, uint32_t options);
void rust_pager_proxy_state_destroy(void* state);
PageSourceProperties rust_pager_proxy_properties(const void* state);
void rust_pager_proxy_send_async_request(const void* state, PageRequest* request);
void rust_pager_proxy_clear_async_request(const void* state, PageRequest* request);
void rust_pager_proxy_swap_async_request(const void* state, PageRequest* old, PageRequest* new_req);
void rust_pager_proxy_on_close(const void* state);
void rust_pager_proxy_on_detach(const void* state);
zx_status_t rust_pager_proxy_wait_on_event(const void* state, Event* event, bool suspendable);
void rust_pager_proxy_dump(const void* state, uint depth, uint32_t max_items);
zx_koid_t rust_pager_proxy_get_koid(const void* state);
void rust_pager_proxy_get_debug_name(const void* state, char (*name)[ZX_MAX_NAME_LEN]);
}

// Page provider implementation that talks to a userspace pager service.
//
// The lifecycle of this class is a little complicated because the pager dispatcher's port
// potentially has an unmanaged reference to the PageSource that contains the PagerProxy through
// packet. Because of this, we need to ensure that the last RefPtr to the PageSource isn't released
// too early when the pager dispatcher gets closed. Normally, the dispatcher can retain its
// reference to the PageSource until the port frees its reference to packet (through the
// PortAllocator). However, if the dispatcher is destroyed, if we can't revoke the port's reference
// to packet, then we end up making the PagerProxy keep a reference to the containing PageSource
// until the packet is freed.
//
// The state and logic of the PagerProxy are implemented in Rust (see pager_proxy.rs). This class is
// a shell that provides the C++ PageProvider interface; the PortPacket, its PortAllocator and the
// complete PageRequest live in the Rust state.
class PagerProxy : public PageProvider {
 public:
  PagerProxy(PagerDispatcher* dispatcher, fbl::RefPtr<PortDispatcher> port, uint64_t key,
             uint32_t options);
  ~PagerProxy() override;

 private:
  // PageProvider methods.
  PageSourceProperties properties() const final {
    return rust_pager_proxy_properties(&opaque_storage_);
  }
  void SendAsyncRequest(PageRequest* request) final {
    rust_pager_proxy_send_async_request(&opaque_storage_, request);
  }
  void ClearAsyncRequest(PageRequest* request) final {
    rust_pager_proxy_clear_async_request(&opaque_storage_, request);
  }
  void SwapAsyncRequest(PageRequest* old, PageRequest* new_req) final {
    rust_pager_proxy_swap_async_request(&opaque_storage_, old, new_req);
  }
  bool DebugIsPageOk(vm_page_t* page, uint64_t offset) final { return true; }
  void OnClose() final { rust_pager_proxy_on_close(&opaque_storage_); }
  void OnDetach() final { rust_pager_proxy_on_detach(&opaque_storage_); }
  zx_status_t WaitOnEvent(Event* event, bool suspendable) final {
    return rust_pager_proxy_wait_on_event(&opaque_storage_, event, suspendable);
  }
  void Dump(uint depth, uint32_t max_items) final {
    rust_pager_proxy_dump(&opaque_storage_, depth, max_items);
  }
  ktl::optional<zx_koid_t> GetKoid() const override {
    return rust_pager_proxy_get_koid(&opaque_storage_);
  }
  void GetDebugName(char (&name)[ZX_MAX_NAME_LEN]) const override {
    rust_pager_proxy_get_debug_name(&opaque_storage_, &name);
  }

  OpaqueStorage<kPagerProxyStateSize, kPagerProxyStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PAGER_PROXY_H_
