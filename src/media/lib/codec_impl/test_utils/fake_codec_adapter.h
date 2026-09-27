// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_LIB_CODEC_IMPL_TEST_UTILS_FAKE_CODEC_ADAPTER_H_
#define SRC_MEDIA_LIB_CODEC_IMPL_TEST_UTILS_FAKE_CODEC_ADAPTER_H_

#include <lib/media/codec_impl/codec_adapter.h>

class FakeCodecAdapter : public CodecAdapter {
 public:
  explicit FakeCodecAdapter(std::mutex& lock, CodecAdapterEvents* codec_adapter_events);
  virtual ~FakeCodecAdapter();

  // CodecAdapter interface:
  bool IsSupportsDynamicBuffers() override;
  uint32_t GetDynamicBuffersMax(CodecPort port) override;
  void CoreCodecSetForceNewBuffersOnNewDimensions(bool force) override;
  std::optional<CoreCodecGetBufferCollectionConstraints3Result>
  CoreCodecGetBufferCollectionConstraints3(CodecPort port) override;
  uint64_t CoreCodecGetConstraintsVersion(CodecPort port) override;
  bool IsCoreCodecRequiringOutputConfigForFormatDetection() override;
  bool IsCoreCodecMappedBufferUseful(CodecPort port) override;
  bool IsCoreCodecHwBased(CodecPort port) override;
  void CoreCodecInit(const fuchsia::media::FormatDetails& initial_input_format_details) override;
  fuchsia_sysmem2::BufferCollectionConstraints CoreCodecGetBufferCollectionConstraints2(
      CodecPort port, const fuchsia::media::StreamBufferConstraints& stream_buffer_constraints,
      const fuchsia::media::StreamBufferPartialSettings& partial_settings) override;
  void CoreCodecSetBufferCollectionInfo(
      CodecPort port, const fuchsia_sysmem2::BufferCollectionInfo& buffer_collection_info) override;
  void CoreCodecStartStream() override;
  void CoreCodecQueueInputFormatDetails(
      const fuchsia::media::FormatDetails& per_stream_override_format_details) override;
  void CoreCodecQueueInputPacket(const CodecPacket* packet) override;
  void CoreCodecQueueInputEndOfStream() override;
  void CoreCodecStopStream() override;
  void CoreCodecAddBuffer(CodecPort port, const CodecBuffer* buffer) override;
  void CoreCodecRemoveBuffer(CodecPort port, const CodecBuffer* buffer) override;
  void CoreCodecConfigureBuffers(CodecPort port,
                                 const std::vector<std::unique_ptr<CodecPacket>>& packets) override;
  void CoreCodecRecycleOutputPacket(CodecPacket* packet) override;
  void CoreCodecEnsureBuffersNotConfigured(CodecPort port) override;
  std::unique_ptr<const fuchsia::media::StreamOutputConstraints> CoreCodecBuildNewOutputConstraints(
      uint64_t stream_lifetime_ordinal, uint64_t new_output_buffer_constraints_version_ordinal,
      bool buffer_constraints_action_required) override;
  fuchsia::media::StreamOutputFormat CoreCodecGetOutputFormat(
      uint64_t stream_lifetime_ordinal,
      uint64_t new_output_format_details_version_ordinal) override;
  void CoreCodecMidStreamOutputBufferReConfigPrepare() override;
  void CoreCodecMidStreamOutputBufferReConfigFinish() override;
  void CoreCodecCloseBufferLifetimeOrdinal(CodecPort port,
                                           uint64_t buffer_lifetime_ordinal) override;

  // Test hooks
  // Must be called prior to CodecImpl::SetCoreCodecAdapter(), which caches
  // IsSupportsDynamicBuffers().
  void SetSupportsDynamicBuffers(bool supports);
  void SetIsCoreCodecRequiringOutputConfigForFormatDetection(bool require);
  void SetIsCoreCodecHwBased(CodecPort port, bool is_hw_based);
  uint64_t IncrementConstraintsVersion(CodecPort port);
  void SetBufferCollectionConstraints(CodecPort port,
                                      fuchsia_sysmem2::BufferCollectionConstraints constraints);
  void SetOnAddBuffer(fit::function<void(CodecPort, const CodecBuffer*)> hook);
  void SetOnRemoveBuffer(fit::function<void(CodecPort, const CodecBuffer*)> hook);
  void SetOnRecycleOutputPacket(fit::function<void(CodecPacket*)> hook);
  void SetOnStartStream(fit::function<void()> hook);
  void SetOnMidStreamOutputBufferReConfigPrepare(fit::function<void()> hook);
  void SetOnEnsureBuffersNotConfigured(fit::function<void(CodecPort)> hook);
  void SetOnCloseBufferLifetimeOrdinal(fit::function<void(CodecPort, uint64_t)> hook);

 private:
  bool supports_dynamic_buffers_ = false;
  bool require_output_config_for_format_detection_ = true;
  bool is_hw_based_[kPortCount] = {false, false};
  uint64_t constraints_version_[kPortCount] = {0, 0};
  std::optional<fuchsia_sysmem2::BufferCollectionConstraints>
      buffer_collection_constraints_[kPortCount];
  fit::function<void(CodecPort, const CodecBuffer*)> on_add_buffer_;
  fit::function<void(CodecPort, const CodecBuffer*)> on_remove_buffer_;
  fit::function<void(CodecPacket*)> on_recycle_output_packet_;
  fit::function<void()> on_start_stream_;
  fit::function<void()> on_mid_stream_output_buffer_re_config_prepare_;
  fit::function<void(CodecPort)> on_ensure_buffers_not_configured_;
  fit::function<void(CodecPort, uint64_t)> on_close_buffer_lifetime_ordinal_;
};

#endif  // SRC_MEDIA_LIB_CODEC_IMPL_TEST_UTILS_FAKE_CODEC_ADAPTER_H_
