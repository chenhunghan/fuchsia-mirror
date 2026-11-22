// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "codec_adapter_mpeg2.h"

#include "device_ctx.h"

namespace amlogic_decoder {

CodecAdapterMpeg2::CodecAdapterMpeg2(std::mutex& lock, CodecAdapterEvents* codec_adapter_events,
                                     DeviceCtx* device)
    : CodecAdapter(lock, codec_adapter_events), device_(device), video_(device_->video()) {
  ZX_DEBUG_ASSERT(device_);
  ZX_DEBUG_ASSERT(video_);
  ZX_DEBUG_ASSERT_MSG(false, "not yet implemented");
  (void)video_;
}

CodecAdapterMpeg2::~CodecAdapterMpeg2() {
  ZX_DEBUG_ASSERT_MSG(false, "not yet implemented - power off probably can go here");
}

bool CodecAdapterMpeg2::IsCoreCodecRequiringOutputConfigForFormatDetection() { return false; }

bool CodecAdapterMpeg2::IsCoreCodecMappedBufferUseful(CodecPort port) {
  // Since protected memory input/output isn't supported for mpeg2, may as well
  // claim we need mapped buffers for now, in case we end up needing to re-pack
  // input or fix output.
  return true;
}

bool CodecAdapterMpeg2::IsCoreCodecHwBased(CodecPort port) { return true; }

void CodecAdapterMpeg2::CoreCodecInit(
    std::shared_ptr<const fuchsia_media::FormatDetails> initial_input_format_details) {
  ZX_DEBUG_ASSERT_MSG(false, "not yet implemented");
}

std::optional<CodecAdapter::CoreCodecGetBufferCollectionConstraints3Result>
CodecAdapterMpeg2::CoreCodecGetBufferCollectionConstraints3(CodecPort port) {
  ZX_DEBUG_ASSERT_MSG(false, "not yet implemented");
  return CoreCodecGetBufferCollectionConstraints3Result{};
}

void CodecAdapterMpeg2::CoreCodecSetBufferCollectionInfo(
    CodecPort port, const fuchsia_sysmem2::BufferCollectionInfo& buffer_collection_info) {
  ZX_DEBUG_ASSERT_MSG(false, "not yet implemented");
}

void CodecAdapterMpeg2::CoreCodecStartStream() {
  ZX_DEBUG_ASSERT_MSG(false, "not yet implemented");
}

void CodecAdapterMpeg2::CoreCodecQueueInputFormatDetails(
    std::shared_ptr<const fuchsia_media::FormatDetails> per_stream_override_format_details) {
  ZX_DEBUG_ASSERT_MSG(false, "not yet implemented");
}

void CodecAdapterMpeg2::CoreCodecQueueInputPacket(const CodecPacket* packet) {
  ZX_DEBUG_ASSERT_MSG(false, "not yet implemented");
}

void CodecAdapterMpeg2::CoreCodecQueueInputEndOfStream() {
  ZX_DEBUG_ASSERT_MSG(false, "not yet implemented");
}

void CodecAdapterMpeg2::CoreCodecStopStream() { ZX_DEBUG_ASSERT_MSG(false, "not yet implemented"); }

void CodecAdapterMpeg2::CoreCodecAddBuffer(CodecPort port, const CodecBuffer* buffer) {
  ZX_DEBUG_ASSERT_MSG(false, "not yet implemented");
}

void CodecAdapterMpeg2::CoreCodecConfigureBuffers(
    CodecPort port, const std::vector<std::unique_ptr<CodecPacket>>& packets) {
  ZX_DEBUG_ASSERT_MSG(false, "not yet implemented");
}

void CodecAdapterMpeg2::CoreCodecRecycleOutputPacket(CodecPacket* packet) {
  ZX_DEBUG_ASSERT_MSG(false, "not yet implemented");
}

void CodecAdapterMpeg2::CoreCodecEnsureBuffersNotConfigured(CodecPort port) {
  ZX_DEBUG_ASSERT_MSG(false, "not yet implemented");
}

fuchsia_media::StreamOutputFormat CodecAdapterMpeg2::CoreCodecGetOutputFormat2(
    uint64_t stream_lifetime_ordinal, uint64_t new_output_format_details_version_ordinal) {
  ZX_DEBUG_ASSERT_MSG(false, "not yet implemented");
  return fuchsia_media::StreamOutputFormat();
}

void CodecAdapterMpeg2::CoreCodecMidStreamOutputBufferReConfigPrepare() {
  ZX_DEBUG_ASSERT_MSG(false, "not yet implemented");
}

void CodecAdapterMpeg2::CoreCodecMidStreamOutputBufferReConfigFinish() {
  ZX_DEBUG_ASSERT_MSG(false, "not yet implemented");
}

}  // namespace amlogic_decoder
