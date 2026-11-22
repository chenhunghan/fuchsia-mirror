// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PAGER_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PAGER_DISPATCHER_H_

#include <lib/object-constants.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <object/dispatcher.h>
#include <object/handle.h>
#include <object/pager_proxy.h>

class PagerDispatcher;
class PortDispatcher;

extern "C" {
zx_status_t cpp_pager_dispatcher_create(
    ffi::Uninitialized<KernelHandle<PagerDispatcher>>* handle_out);

void cpp_pager_proxy_free(PagerProxy* proxy);
void* cpp_pager_proxy_get_ref_counted(PagerProxy* proxy);
zx_status_t cpp_pager_proxy_create(PagerDispatcher* dispatcher, PortDispatcher* port, uint64_t key,
                                   uint32_t options, PagerProxy** out_proxy);
PageProvider* cpp_pager_proxy_as_page_provider(PagerProxy* proxy);

void rust_pager_dispatcher_state_init(void* state, const PagerDispatcher* disp);
void rust_pager_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_pager_dispatcher_state_get_lock(const void* state);
void rust_pager_dispatcher_on_zero_handles(const PagerDispatcher* disp);
}

class PagerDispatcher final : public Dispatcher {
 public:
  ~PagerDispatcher() final;

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_PAGER; }
  zx_koid_t get_related_koid() const final { return ZX_KOID_INVALID; }
  bool is_waitable() const final { return false; }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final {
    return UserSignalSelfSolo(this, clear_mask, set_mask, 0);
  }
  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

  void on_zero_handles() final { rust_pager_dispatcher_on_zero_handles(this); }

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  friend zx_status_t cpp_pager_dispatcher_create(
      ffi::Uninitialized<KernelHandle<PagerDispatcher>>* handle_out);

  explicit PagerDispatcher();
  DISALLOW_COPY_ASSIGN_AND_MOVE(PagerDispatcher);

  OpaqueStorage<kPagerDispatcherStateSize, kPagerDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PAGER_DISPATCHER_H_
