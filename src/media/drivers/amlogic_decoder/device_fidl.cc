// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "device_fidl.h"

#include <lib/media/codec_impl/codec_admission_control.h>
#include <lib/sync/cpp/completion.h>

#include "device_ctx.h"

namespace amlogic_decoder {

DeviceFidl::DeviceFidl(DeviceCtx* device) : device_(device) {}

void DeviceFidl::InitializeClosureQueue() {
  ZX_DEBUG_ASSERT(!closure_queue_initialized_);
  libsync::Completion completion;
  device_->driver()->PostToSharedFidl([this, &completion]() {
    closure_queue_.SetDispatcher(device_->driver()->shared_fidl_loop()->dispatcher());
    closure_queue_initialized_ = true;
    completion.Signal();
  });
  completion.Wait();
}

DeviceFidl::~DeviceFidl() {
  if (!closure_queue_initialized_) {
    ZX_ASSERT(factories_.empty());
    ZX_ASSERT(codecs_.empty());
    return;
  }
  // The DeviceCtx should have already moved over to the shared fidl sequence
  // for this (if ~DeviceCtx implemented), else it's not safe to ~fidl::Binding.
  //
  // Also, DeviceFidl::ConnectChannelBoundCodecFactory() relies on ability to
  // post work which will run on the shared fidl sequence before ~DeviceFidl()
  // runs on the shared fidl sequence.
  ZX_ASSERT(device_->driver()->IsSharedFidlSynchronized());
  closure_queue_.StopAndClear();
}

void DeviceFidl::ConnectChannelBoundCodecFactory(zx::channel request) {
  // Any destruction of "this" is also posted over to the shared fidl sequence,
  // and will run after the work posted here runs.
  //
  // This posting over to the shared fidl sequence is mainly for the benefit of
  // factories_ only being touched from that sequence, and secondarily to avoid
  // taking a dependency on Bind() working from a different thread/sequence
  // (both in Bind() and in DeviceFidl code).
  closure_queue_.Enqueue([this, server_endpoint = std::move(request)]() mutable {
    ZX_ASSERT(device_->driver()->IsSharedFidlSynchronized());
    auto factory = std::make_unique<LocalCodecFactory>(device_);
    LocalCodecFactory* raw_factory_ptr = factory.get();
    factory->SetErrorHandler([this, raw_factory_ptr] { DeleteFactory(raw_factory_ptr); });
    auto insert_result = factories_.insert(std::make_pair(raw_factory_ptr, std::move(factory)));
    // insert success
    ZX_DEBUG_ASSERT(insert_result.second);
    insert_result.first->second->Bind(std::move(server_endpoint));
  });
}

void DeviceFidl::BindCodecImpl(std::unique_ptr<CodecImpl> codec) {
  ZX_ASSERT(device_->driver()->IsSharedFidlSynchronized());
  CodecImpl* raw_codec_ptr = codec.get();
  auto insert_result = codecs_.insert(std::make_pair(raw_codec_ptr, std::move(codec)));
  // insert success
  ZX_DEBUG_ASSERT(insert_result.second);
  (*insert_result.first).second->BindAsync([this, raw_codec_ptr] {
    ZX_ASSERT(device_->driver()->IsSharedFidlSynchronized());
    auto iter = codecs_.find(raw_codec_ptr);
    ZX_DEBUG_ASSERT(iter != codecs_.end());
    codecs_.erase(iter);
  });
}

void DeviceFidl::DeleteFactory(LocalCodecFactory* raw_factory_ptr) {
  ZX_ASSERT(device_->driver()->IsSharedFidlSynchronized());
  auto iter = factories_.find(raw_factory_ptr);
  ZX_DEBUG_ASSERT(iter != factories_.end());
  factories_.erase(iter);
}

}  // namespace amlogic_decoder
