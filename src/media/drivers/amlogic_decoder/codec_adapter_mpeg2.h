// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_DRIVERS_AMLOGIC_DECODER_CODEC_ADAPTER_MPEG2_H_
#define SRC_MEDIA_DRIVERS_AMLOGIC_DECODER_CODEC_ADAPTER_MPEG2_H_

#include <fidl/fuchsia.media/cpp/fidl.h>
#include <fidl/fuchsia.sysmem2/cpp/fidl.h>
#include <lib/media/codec_impl/codec_adapter.h>
#include <zircon/compiler.h>

#include <optional>

namespace amlogic_decoder {

class DeviceCtx;
class AmlogicVideo;
class CodecAdapterMpeg2 : public CodecAdapter {
 public:
  explicit CodecAdapterMpeg2(std::mutex& lock, CodecAdapterEvents* codec_adapter_events,
                             DeviceCtx* device);
  ~CodecAdapterMpeg2();

  bool IsCoreCodecRequiringOutputConfigForFormatDetection() override;
  bool IsCoreCodecMappedBufferUseful(CodecPort port) override;
  bool IsCoreCodecHwBased(CodecPort port) override;

  void CoreCodecInit(
      std::shared_ptr<const fuchsia_media::FormatDetails> initial_input_format_details) override;
  std::optional<CoreCodecGetBufferCollectionConstraints3Result>
  CoreCodecGetBufferCollectionConstraints3(CodecPort port) override;
  void CoreCodecSetBufferCollectionInfo(
      CodecPort port, const fuchsia_sysmem2::BufferCollectionInfo& buffer_collection_info) override;
  void CoreCodecStartStream() override;
  void CoreCodecQueueInputFormatDetails(std::shared_ptr<const fuchsia_media::FormatDetails>
                                            per_stream_override_format_details) override;
  void CoreCodecQueueInputPacket(const CodecPacket* packet) override;
  void CoreCodecQueueInputEndOfStream() override;
  void CoreCodecStopStream() override;
  void CoreCodecAddBuffer(CodecPort port, const CodecBuffer* buffer) override;
  void CoreCodecConfigureBuffers(CodecPort port,
                                 const std::vector<std::unique_ptr<CodecPacket>>& packets) override;
  void CoreCodecRecycleOutputPacket(CodecPacket* packet) override;
  void CoreCodecEnsureBuffersNotConfigured(CodecPort port) override;
  fuchsia_media::StreamOutputFormat CoreCodecGetOutputFormat2(
      uint64_t stream_lifetime_ordinal,
      uint64_t new_output_format_details_version_ordinal) override;
  void CoreCodecMidStreamOutputBufferReConfigPrepare() override;
  void CoreCodecMidStreamOutputBufferReConfigFinish() override;

 private:
  DeviceCtx* device_ = nullptr;
  AmlogicVideo* video_ = nullptr;

  CodecAdapterMpeg2() = delete;
  DISALLOW_COPY_ASSIGN_AND_MOVE(CodecAdapterMpeg2);
};

}  // namespace amlogic_decoder

#endif  // SRC_MEDIA_DRIVERS_AMLOGIC_DECODER_CODEC_ADAPTER_MPEG2_H_
