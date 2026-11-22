// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_CODEC_CODECS_VAAPI_LOCAL_SINGLE_CODEC_FACTORY_H_
#define SRC_MEDIA_CODEC_CODECS_VAAPI_LOCAL_SINGLE_CODEC_FACTORY_H_

#include <fidl/fuchsia.mediacodec/cpp/fidl.h>
#include <fidl/fuchsia.sysmem2/cpp/fidl.h>
#include <lib/closure-queue/closure_queue.h>
#include <lib/media/codec_impl/codec_adapter.h>
#include <lib/media/codec_impl/codec_admission_control.h>
#include <lib/media/codec_impl/codec_diagnostics.h>
#include <lib/media/codec_impl/codec_impl.h>
#include <lib/syslog/cpp/macros.h>

#include <type_traits>

#include "vaapi_utils.h"

// Marker type to specify these is no adapter to serve a request.
class NoAdapter {};

// Prepares a single codec for the codec runner and then requests drop of self.
// If a software can only provide an encoder or decoder, the other should be
// assigned NoAdapter in the template arguments, e.g.:
//   LocalSingleCodecFactory<CodecAdapterFfmpeg, NoAdapter>
template <typename DecoderAdapter, typename EncoderAdapter>
class LocalSingleCodecFactory : public fidl::Server<fuchsia_mediacodec::CodecFactory> {
 public:
  LocalSingleCodecFactory(async_dispatcher_t* fidl_dispatcher,
                          fidl::ClientEnd<fuchsia_sysmem2::Allocator> sysmem,
                          fidl::ServerEnd<fuchsia_mediacodec::CodecFactory> request,
                          fit::function<void(std::unique_ptr<CodecImpl>)> factory_done_callback,
                          CodecAdmissionControl* codec_admission_control,
                          fit::function<void(zx_status_t)> error_handler,
                          CodecDiagnostics* codec_diagnostics = nullptr)
      : fidl_dispatcher_(fidl_dispatcher),
        sysmem_(std::move(sysmem)),
        error_handler_(std::move(error_handler)),
        factory_done_callback_(std::move(factory_done_callback)),
        codec_admission_control_(codec_admission_control),
        codec_diagnostics_(codec_diagnostics),
        closure_queue_(std::make_shared<ClosureQueue>(fidl_dispatcher)),
        binding_(fidl_dispatcher, std::move(request), this, [this](fidl::UnbindInfo info) {
          if (auto handler = std::move(error_handler_)) {
            handler(info.status());
          }
        }) {
    (void)fidl::SendEvent(binding_)->OnCodecList(GetDeprecatedCodecList());
  }

  ~LocalSingleCodecFactory() override { closure_queue_->StopAndClear(); }

  void GetDetailedCodecDescriptions(
      GetDetailedCodecDescriptionsCompleter::Sync& completer) override {
    completer.Reply({{.codecs = GetCodecDescriptions()}});
  }

  void CreateDecoder(CreateDecoderRequest& request,
                     CreateDecoderCompleter::Sync& completer) override {
    VendCodecAdapter<DecoderAdapter>(std::move(request.decoder_params()),
                                     std::move(request.decoder()), std::move(lifetime_tracking_));
    ZX_DEBUG_ASSERT(lifetime_tracking_.empty());
  }

  void CreateEncoder(CreateEncoderRequest& request,
                     CreateEncoderCompleter::Sync& completer) override {
    VendCodecAdapter<EncoderAdapter>(std::move(request.encoder_params()),
                                     std::move(request.encoder()), std::move(lifetime_tracking_));
    ZX_DEBUG_ASSERT(lifetime_tracking_.empty());
  }

  void AttachLifetimeTracking(AttachLifetimeTrackingRequest& request,
                              AttachLifetimeTrackingCompleter::Sync& completer) override {
    ZX_DEBUG_ASSERT(lifetime_tracking_.size() <=
                    fuchsia_mediacodec::kCodecFactoryLifetimeTrackingEventpairPerCreateMax);
    if (lifetime_tracking_.size() >=
        fuchsia_mediacodec::kCodecFactoryLifetimeTrackingEventpairPerCreateMax) {
      // Ensure the on_unbound callback tears down the factory and quits the isolate loop once
      // dispatch finishes, even if a prior pipelined CreateDecoder/CreateEncoder cleared
      // error_handler_.
      error_handler_ = [this](zx_status_t) {
        auto factory_done_callback = std::move(factory_done_callback_);
        if (factory_done_callback) {
          factory_done_callback(nullptr);
        }
      };
      completer.Close(ZX_ERR_BAD_STATE);
      return;
    }
    lifetime_tracking_.emplace_back(std::move(request.codec_end()));
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_mediacodec::CodecFactory> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    // See src/media/codec/factory/codec_factory_impl.cc handle_unknown_method for why we handle
    // this way.
    if (metadata.unknown_method_type == fidl::UnknownMethodType::kOneWay) {
      // As a client author, if you see this message, please read the comments
      // in src/media/codec/factory/codec_factory_impl.cc handle_unknown_method.
      FX_LOGS(WARNING) << "Unrecognized one-way message - ordinal: " << std::hex
                       << metadata.method_ordinal;
    } else {
      FX_LOGS(INFO) << "Unrecognized two-way message - ordinal: " << std::hex
                    << metadata.method_ordinal;
    }
  }

 private:
  template <typename Adapter, typename Params>
  void VendCodecAdapter(Params params,
                        fidl::ServerEnd<fuchsia_media::StreamProcessor> codec_request,
                        std::vector<zx::eventpair> lifetime_tracking_eventpair) {
    if constexpr (std::is_same_v<Adapter, NoAdapter>) {
      // No adapter.
      // ~codec_request.
      // ~lifetime_tracking_eventpair
      return;
    } else {
      // Ignore channel errors (e.g. PEER_CLOSED) after this point, because this channel has served
      // its purpose. Otherwise the error handler could tear down the loop before the codec was
      // finished being added.
      error_handler_ = nullptr;

      // `TryAddCodec` runs asynchronously and cannot be cancelled, while `binding_` remains bound
      // in the meantime. Pipelined client messages (e.g. a second `CreateDecoder`/`CreateEncoder`,
      // or exceeding the `AttachLifetimeTracking` limit) can lead to `factory_done_callback_`
      // destroying `this` (via the `ServerBinding` close handler, which runs asynchronously after
      // the triggering dispatch returns) before a pending `TryAddCodec` callback runs. A pending
      // `TryAddCodec` callback can also run first, before that teardown. Routing `callback`
      // through `closure_queue_` (stopped in `~LocalSingleCodecFactory()`) ensures that if `this`
      // is destroyed first, `callback` is dropped rather than invoked with a dangling `this`
      // pointer.
      auto callback = [this, params = std::move(params), codec_request = std::move(codec_request),
                       lifetime_tracking_eventpair = std::move(lifetime_tracking_eventpair)](
                          std::unique_ptr<CodecAdmission> codec_admission) mutable {
        if (!codec_admission) {
          // ~codec_request closes channel.
          return;
        }

        if (!sysmem_.is_valid()) {
          FX_LOGS(WARNING) << "VendCodecAdapter() only meant to be used once per "
                              "LocalSingleCodecFactory\n";
          // ~codec_request closes channel.
          return;
        }

        auto codec_impl = std::make_unique<CodecImpl>(std::move(sysmem_),
                                                      std::move(codec_admission), fidl_dispatcher_,
                                                      std::move(params), std::move(codec_request));

        codec_impl->SetLifetimeTracking(std::move(lifetime_tracking_eventpair));

        codec_impl->SetCoreCodecAdapter(
            std::make_unique<Adapter>(codec_impl->lock(), codec_impl.get()));

        if (codec_diagnostics_) {
          codec_impl->SetCodecDiagnostics(codec_diagnostics_);
        }

        // This hands off the codec impl to the creator of |this| and is
        // expected to |~this|. Move the callback to the stack so its inline
        // captures remain valid after |this| is destroyed.
        auto factory_done_callback = std::move(factory_done_callback_);
        factory_done_callback(std::move(codec_impl));
      };

      codec_admission_control_->TryAddCodec(
          /*multi_instance=*/true, [closure_queue = closure_queue_, callback = std::move(callback)](
                                       std::unique_ptr<CodecAdmission> codec_admission) mutable {
            closure_queue->Enqueue([callback = std::move(callback),
                                    codec_admission = std::move(codec_admission)]() mutable {
              callback(std::move(codec_admission));
            });
          });
    }
  }

  async_dispatcher_t* fidl_dispatcher_;
  fidl::ClientEnd<fuchsia_sysmem2::Allocator> sysmem_;
  fit::function<void(zx_status_t)> error_handler_;
  // Returns the codec implementation and requests drop of self.
  fit::function<void(std::unique_ptr<CodecImpl>)> factory_done_callback_;
  CodecAdmissionControl* codec_admission_control_;
  std::vector<zx::eventpair> lifetime_tracking_;
  CodecDiagnostics* codec_diagnostics_;
  std::shared_ptr<ClosureQueue> closure_queue_;
  fidl::ServerBinding<fuchsia_mediacodec::CodecFactory> binding_;
};

#endif  // SRC_MEDIA_CODEC_CODECS_VAAPI_LOCAL_SINGLE_CODEC_FACTORY_H_
