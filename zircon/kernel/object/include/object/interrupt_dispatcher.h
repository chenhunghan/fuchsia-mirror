// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_INTERRUPT_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_INTERRUPT_DISPATCHER_H_

#include <lib/object-constants.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <object/dispatcher.h>

class InterruptDispatcher;

extern "C" {

InterruptDispatcher* cpp_interrupt_dispatcher_create(uint32_t flags, uint32_t options);

void rust_interrupt_dispatcher_state_init(void* state, const InterruptDispatcher* disp,
                                          uint32_t flags, uint32_t options);
void rust_interrupt_dispatcher_teardown(const InterruptDispatcher* disp);
void rust_interrupt_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_interrupt_dispatcher_state_get_lock(const void* state);
void rust_interrupt_dispatcher_on_zero_handles(const InterruptDispatcher* disp);
bool rust_interrupt_dispatcher_is_virtual(const InterruptDispatcher* disp);

}  // extern "C"

class InterruptDispatcher final : public Dispatcher {
 public:
  ~InterruptDispatcher() final;
  InterruptDispatcher& operator=(const InterruptDispatcher&) = delete;

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_INTERRUPT; }
  zx_koid_t get_related_koid() const final { return ZX_KOID_INVALID; }
  bool is_waitable() const final { return true; }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final {
    return UserSignalSelfSolo(
        this, clear_mask, set_mask,
        rust_interrupt_dispatcher_is_virtual(this) ? ZX_VIRTUAL_INTERRUPT_UNTRIGGERED : 0);
  }
  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

  void on_zero_handles() final { rust_interrupt_dispatcher_on_zero_handles(this); }

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  friend InterruptDispatcher* cpp_interrupt_dispatcher_create(uint32_t flags, uint32_t options);

  InterruptDispatcher(uint32_t flags, uint32_t options);

  OpaqueStorage<kInterruptDispatcherStateSize, kInterruptDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_INTERRUPT_DISPATCHER_H_
