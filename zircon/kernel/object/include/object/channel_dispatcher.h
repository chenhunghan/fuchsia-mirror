// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_CHANNEL_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_CHANNEL_DISPATCHER_H_

#include <lib/object-constants.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <object/dispatcher.h>
#include <object/handle.h>

class ChannelDispatcher;

DECLARE_PEERED_DISPATCHER_RUST_PROTOS(ChannelDispatcher, rust_channel_dispatcher)

extern "C" {
zx_status_t cpp_channel_dispatcher_create(
    void* holder, ffi::Uninitialized<KernelHandle<ChannelDispatcher>>* handle_out);
}  // extern "C"

class ChannelDispatcher final : public Dispatcher {
 public:
  ~ChannelDispatcher() final;

  DECLARE_PEERED_DISPATCHER_RUST_METHODS(rust_channel_dispatcher, ZX_OBJ_TYPE_CHANNEL, true)

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  friend zx_status_t cpp_channel_dispatcher_create(
      void* holder, ffi::Uninitialized<KernelHandle<ChannelDispatcher>>* handle_out);
  explicit ChannelDispatcher(void* holder);

  OpaqueStorage<kChannelDispatcherStateSize, kChannelDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_CHANNEL_DISPATCHER_H_
