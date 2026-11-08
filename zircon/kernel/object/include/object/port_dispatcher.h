// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PORT_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PORT_DISPATCHER_H_

#include <lib/object-constants.h>
#include <stddef.h>
#include <zircon/rights.h>
#include <zircon/syscalls/port.h>
#include <zircon/types.h>

#include <fbl/intrusive_double_list.h>
#include <kernel/ffi.h>
#include <kernel/mutex.h>
#include <object/dispatcher.h>
#include <object/handle.h>
#include <object/signal_observer.h>

// PortObserver lifetime and locking notes: the canonical description of the
// PortDispatcher / PortObserver / Dispatcher pointer graph, cancelation
// ordering, and lock hierarchy lives in the module documentation at the top of
// port_dispatcher.rs.

class PortDispatcher;
class PortObserver;
class PortSignalObserver;
struct PortPacket;

struct PortAllocator {
  void (*free_fn)(const PortAllocator* self, PortPacket* packet);
};
static_assert(sizeof(PortAllocator) == 8);
static_assert(alignof(PortAllocator) == 8);
static_assert(offsetof(PortAllocator, free_fn) == 0);

struct PortPacket final : public fbl::DoublyLinkedListable<PortPacket*> {
  zx_port_packet_t packet = {};
  const void* const handle = nullptr;
  PortObserver* observer = nullptr;
  PortAllocator* const allocator = nullptr;

  constexpr PortPacket() = default;
  constexpr PortPacket(const void* handle, PortAllocator* allocator)
      : handle(handle), allocator(allocator) {}
  PortPacket(const PortPacket&) = delete;
  void operator=(PortPacket) = delete;
};

extern "C" {

void rust_port_dispatcher_state_init(void* state, const void* disp, uint32_t options);
void rust_port_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_port_dispatcher_state_get_lock(const void* state);
void rust_port_dispatcher_on_zero_handles(const PortDispatcher* disp);
zx_status_t rust_port_dispatcher_queue(const PortDispatcher* disp, PortPacket* port_packet);
bool rust_port_dispatcher_cancel_queued_packet(const PortDispatcher* disp, PortPacket* port_packet);

zx_status_t cpp_port_dispatcher_create(uint32_t options,
                                       ffi::Uninitialized<KernelHandle<PortDispatcher>>* handle_out,
                                       ffi::Uninitialized<zx_rights_t>* rights_out);
void cpp_port_signal_observer_init(ffi::Uninitialized<PortSignalObserver>* observer,
                                   uint32_t options);
void cpp_port_signal_observer_destroy(PortSignalObserver* observer);

}  // extern "C"

// The PortDispatcher implements the port kernel object which is the cornerstone
// for waiting on object changes in Zircon. The PortDispatcher handles 3 usage
// cases:
//  1- Object state change notification: zx_object_wait_async()
//  2- Manual queuing: zx_port_queue()
//  3- Interrupt change notification: zx_interrupt_bind()
//
// The state and queue logic live in Rust (port_dispatcher.rs).
class PortDispatcher final : public Dispatcher {
 public:
  static constexpr zx_rights_t default_rights() { return ZX_DEFAULT_PORT_RIGHTS; }
  static zx_status_t Create(uint32_t options, KernelHandle<PortDispatcher>* handle,
                            zx_rights_t* rights);

  ~PortDispatcher() final;
  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_PORT; }

  void on_zero_handles() final { rust_port_dispatcher_on_zero_handles(this); }

  bool is_waitable() const final { return default_rights() & ZX_RIGHT_WAIT; }
  zx_koid_t get_related_koid() const final { return ZX_KOID_INVALID; }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final {
    return UserSignalSelfSolo(this, clear_mask, set_mask, 0);
  }
  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

  zx_status_t Queue(PortPacket* port_packet) const {
    return rust_port_dispatcher_queue(this, port_packet);
  }
  bool CancelQueued(PortPacket* port_packet) const {
    return rust_port_dispatcher_cancel_queued_packet(this, port_packet);
  }

  // Creates an observer registered with this port.
  //
  // Requires that a read lock is held on the handle table containing |handle|.
  zx::result<SignalObserver*> MakeObserver(uint32_t options, const Handle* handle, uint64_t key,
                                           zx_signals_t signals) const;

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  friend zx_status_t cpp_port_dispatcher_create(
      uint32_t options, ffi::Uninitialized<KernelHandle<PortDispatcher>>* handle_out,
      ffi::Uninitialized<zx_rights_t>* rights_out);
  explicit PortDispatcher(uint32_t options);

  OpaqueStorage<kPortDispatcherStateSize, kPortDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PORT_DISPATCHER_H_
