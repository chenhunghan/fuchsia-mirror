// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_DRIVERS_AMLOGIC_DECODER_LOCAL_CODEC_FACTORY_H_
#define SRC_MEDIA_DRIVERS_AMLOGIC_DECODER_LOCAL_CODEC_FACTORY_H_

#include <fidl/fuchsia.mediacodec/cpp/fidl.h>
#include <lib/closure-queue/closure_queue.h>
#include <lib/fit/function.h>

#include <memory>
#include <optional>
#include <vector>

#include <fbl/macros.h>

#include "lib/zx/eventpair.h"

namespace amlogic_decoder {

// TODO(dustingreen): Concider pulling LocalCodecFactory out into a source_set
// that can be used by other HW codec drivers (in contrast to CodecImpl source
// set which will need to remain usable for both SW and HW codecs).

class DeviceCtx;

// A LocalCodecFactory is owned by DeviceFidl via a unique_ptr<>.  The channel
// closing can also mandate that DeviceFidl stop owning the LocalCodecFactory.
//
// Unlike a SW codec isolate's local CodecFactory, this HW codec's local
// CodecFactory doesn't self-close after creating one codec.
class LocalCodecFactory : public fidl::Server<fuchsia_mediacodec::CodecFactory> {
 public:
  // Must run on the shared fidl sequence.
  //
  // device - parent device.
  explicit LocalCodecFactory(DeviceCtx* device);

  // Must run on the shared fidl sequence.
  ~LocalCodecFactory() override;

  // This needs to be called before Bind(), not after.  The caller's
  // error_handler is called up to once, when/if the channel has an error.
  //
  // If ~LocalCodecFactory() runs before the channel has any error, then
  // error_handler won't be run.
  //
  // Must run on the shared fidl sequence.
  //
  // The error_handler runs on the shared fidl sequence, and never while
  // SetErrorHandler() is still on the stack.
  void SetErrorHandler(fit::closure error_handler);

  // Until this is called, the LocalCodecFactory won't do anything itself.
  // During/after this call and until destruction, the LocalCodecFactory can
  // create CodecImpl instances on client request(s) using the shared fidl
  // sequence, and can call error_handler on the shared fidl sequence.
  //
  // Dispatching of CodecFactory interface methods will occur on the shared fidl
  // sequence.
  //
  // Must run on the shared fidl sequence.
  void Bind(zx::channel server_endpoint);

  //
  // CodecFactory interface
  //

  void GetDetailedCodecDescriptions(
      GetDetailedCodecDescriptionsCompleter::Sync& completer) override;

  void CreateDecoder(CreateDecoderRequest& request,
                     CreateDecoderCompleter::Sync& completer) override;

  void CreateEncoder(CreateEncoderRequest& request,
                     CreateEncoderCompleter::Sync& completer) override;

  void AttachLifetimeTracking(AttachLifetimeTrackingRequest& request,
                              AttachLifetimeTrackingCompleter::Sync& completer) override;

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_mediacodec::CodecFactory> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override;

 private:
  DeviceCtx* device_ = nullptr;

  fit::closure error_handler_;

  // This binding doesn't channel-own this LocalCodecFactory.  The DeviceFidl
  // owns all the LocalCodecFactory(s).  The DeviceFidl will SetErrorHandler()
  // such that its ownership drops if the channel fails.
  std::optional<fidl::ServerBinding<fuchsia_mediacodec::CodecFactory>> factory_binding_;

  bool is_error_handler_set_ = false;

  std::vector<zx::eventpair> lifetime_tracking_;

  // We use std::shared_ptr here so that asynchronous callbacks (specifically TryAddCodec's callback
  // which queues task execution on the dispatcher) can capture a shared reference to the queue.
  // This guarantees the ClosureQueue remains allocated even if the LocalCodecFactory is destroyed
  // synchronously. StopAndClear() is called in the destructor to ensure any callbacks enqueued
  // post-destruction are safely discarded.
  std::shared_ptr<ClosureQueue> closure_queue_;

  DISALLOW_COPY_ASSIGN_AND_MOVE(LocalCodecFactory);
};

}  // namespace amlogic_decoder

#endif  // SRC_MEDIA_DRIVERS_AMLOGIC_DECODER_LOCAL_CODEC_FACTORY_H_
