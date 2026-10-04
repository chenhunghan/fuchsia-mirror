// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_CHANNEL_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_CHANNEL_DISPATCHER_H_

#include <lib/object-constants.h>
#include <stdint.h>
#include <zircon/types.h>

#include <kernel/owned_wait_queue.h>
#include <object/dispatcher.h>
#include <object/handle.h>
#include <object/message_packet.h>
#include <object/opaque_storage.h>

class ChannelDispatcher;

DECLARE_PEERED_DISPATCHER_RUST_PROTOS(ChannelDispatcher, rust_channel_dispatcher)

extern "C" {
zx_status_t rust_channel_dispatcher_create(KernelHandle<ChannelDispatcher>* handle0,
                                           KernelHandle<ChannelDispatcher>* handle1,
                                           zx_rights_t* rights);
zx_status_t rust_channel_dispatcher_write(const ChannelDispatcher* disp, zx_koid_t owner,
                                          MessagePacket* msg);
void rust_channel_dispatcher_set_owner(const ChannelDispatcher* disp, zx_koid_t new_owner);
bool rust_channel_dispatcher_peer_has_closed(const ChannelDispatcher* disp);
void rust_channel_dispatcher_get_message_counts(const ChannelDispatcher* disp, uint64_t* current,
                                                uint64_t* max);
int64_t rust_channel_dispatcher_get_channel_full_count();
void rust_message_waiter_init(void* waiter);
void rust_message_waiter_destroy(void* waiter);
}  // extern "C"

class ChannelDispatcher final : public Dispatcher {
 public:
  struct MessageCounts {
    uint64_t current;
    uint64_t max;
  };

  static zx_status_t Create(KernelHandle<ChannelDispatcher>* handle0,
                            KernelHandle<ChannelDispatcher>* handle1, zx_rights_t* rights) {
    return rust_channel_dispatcher_create(handle0, handle1, rights);
  }

  static int64_t get_channel_full_count() {
    return rust_channel_dispatcher_get_channel_full_count();
  }

  explicit ChannelDispatcher(void* holder);
  ~ChannelDispatcher() final;

  DECLARE_PEERED_DISPATCHER_RUST_METHODS(rust_channel_dispatcher, ZX_OBJ_TYPE_CHANNEL, true)

  zx_status_t Write(zx_koid_t owner, MessagePacketPtr msg) const {
    return rust_channel_dispatcher_write(this, owner, msg.release());
  }

  void set_owner(zx_koid_t new_owner) final { rust_channel_dispatcher_set_owner(this, new_owner); }

  bool PeerHasClosed() const { return rust_channel_dispatcher_peer_has_closed(this); }

  MessageCounts get_message_counts() const {
    MessageCounts counts{};
    rust_channel_dispatcher_get_message_counts(this, &counts.current, &counts.max);
    return counts;
  }

  class MessageWaiter {
   public:
    MessageWaiter() { rust_message_waiter_init(&opaque_storage_); }
    ~MessageWaiter() { rust_message_waiter_destroy(&opaque_storage_); }

    MessageWaiter(const MessageWaiter&) = delete;
    MessageWaiter& operator=(const MessageWaiter&) = delete;
    MessageWaiter(MessageWaiter&&) = delete;
    MessageWaiter& operator=(MessageWaiter&&) = delete;

   private:
    OpaqueStorage<kMessageWaiterSize, kMessageWaiterAlign> opaque_storage_;
  };

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  OpaqueStorage<kChannelDispatcherStateSize, kChannelDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_CHANNEL_DISPATCHER_H_
