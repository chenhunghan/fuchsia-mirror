// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "fake_codec_adapter.h"

#include <lib/media/codec_impl/codec_packet.h>
#include <zircon/assert.h>

#include <limits>

namespace {

// We use "video/raw" for output since for now it makes sense to pretend to be a
// video decoder.
constexpr const char* kOutputMimeType = "video/raw";
constexpr uint32_t kCodedWidth = 256;
constexpr uint32_t kCodedHeight = 144;
constexpr uint32_t kPixelStride = sizeof(uint32_t);
constexpr uint32_t kBytesPerRow = kCodedWidth * kPixelStride;
constexpr uint32_t kDisplayWidth = kCodedWidth;
constexpr uint32_t kDisplayHeight = kCodedHeight;

constexpr uint32_t kInputMinBufferCountForCamping = 1;
constexpr uint32_t kOutputMinBufferCountForCamping = 5;

constexpr uint32_t kPerPacketBufferBytesMin = kBytesPerRow * kCodedHeight;
constexpr uint32_t kDynamicBuffersMax = 64;

}  // namespace

FakeCodecAdapter::FakeCodecAdapter(std::mutex& lock, CodecAdapterEvents* codec_adapter_events)
    : CodecAdapter(lock, codec_adapter_events) {
  // nothing else to do here
}

FakeCodecAdapter::~FakeCodecAdapter() {
  // nothing to do here
}

bool FakeCodecAdapter::IsSupportsDynamicBuffers() { return supports_dynamic_buffers_; }

uint32_t FakeCodecAdapter::GetDynamicBuffersMax(CodecPort port) {
  return supports_dynamic_buffers_ ? kDynamicBuffersMax : 0;
}

void FakeCodecAdapter::CoreCodecSetForceNewBuffersOnNewDimensions(bool force) {
  // nothing to do here
}

std::optional<CodecAdapter::CoreCodecGetBufferCollectionConstraints3Result>
FakeCodecAdapter::CoreCodecGetBufferCollectionConstraints3(CodecPort port) {
  CoreCodecGetBufferCollectionConstraints3Result result;
  result.constraints_version = constraints_version_[port];

  // If test harness has set an override, just return that.
  if (buffer_collection_constraints_[port]) {
    result.constraints = *buffer_collection_constraints_[port];
    return result;
  }

  fuchsia_sysmem2::BufferCollectionConstraints constraints;
  ZX_DEBUG_ASSERT(!constraints.usage().has_value());
  if (port == kInputPort) {
    constraints.min_buffer_count_for_camping() = kInputMinBufferCountForCamping;
  } else {
    ZX_DEBUG_ASSERT(port == kOutputPort);
    constraints.min_buffer_count_for_camping() = kOutputMinBufferCountForCamping;
  }
  ZX_DEBUG_ASSERT(!constraints.min_buffer_count_for_dedicated_slack().has_value());
  ZX_DEBUG_ASSERT(!constraints.min_buffer_count_for_shared_slack().has_value());
  ZX_DEBUG_ASSERT(!constraints.min_buffer_count().has_value());
  // un-set is treated as 0xFFFFFFFF.
  ZX_DEBUG_ASSERT(!constraints.max_buffer_count().has_value());
  auto& bmc = constraints.buffer_memory_constraints().emplace();
  if (port == kInputPort) {
    // Despite the defaults being fine for the fake, CodecImpl wants
    // buffer_memory_constraints to have a value.  All real CodecAdapter
    // implementations will likely want to have some constraints on buffer size
    // - or if they don't, they can also just emplace buffer_memory_constraints
    // with defaults for all fields.
    ZX_DEBUG_ASSERT(constraints.buffer_memory_constraints().has_value());
  } else {
    bmc.min_size_bytes() = kPerPacketBufferBytesMin;
    bmc.cpu_domain_supported() = true;
  }
  ZX_DEBUG_ASSERT(!constraints.image_format_constraints().has_value());
  result.constraints = std::move(constraints);
  return result;
}

uint64_t FakeCodecAdapter::CoreCodecGetConstraintsVersion(CodecPort port) {
  return constraints_version_[port];
}

bool FakeCodecAdapter::IsCoreCodecRequiringOutputConfigForFormatDetection() {
  // To cause CodecImpl to send OnOutputConstraints before format detection.
  return require_output_config_for_format_detection_;
}

bool FakeCodecAdapter::IsCoreCodecMappedBufferUseful(CodecPort port) { return true; }

bool FakeCodecAdapter::IsCoreCodecHwBased(CodecPort port) { return is_hw_based_[port]; }

void FakeCodecAdapter::CoreCodecInit(
    std::shared_ptr<const fuchsia_media::FormatDetails> initial_input_format_details) {
  // nothing to do here
}

void FakeCodecAdapter::CoreCodecSetBufferCollectionInfo(
    CodecPort port, const fuchsia_sysmem2::BufferCollectionInfo& buffer_collection_info) {
  // nothing to do here
}

void FakeCodecAdapter::CoreCodecStartStream() {
  if (on_start_stream_) {
    on_start_stream_();
  }
}

void FakeCodecAdapter::CoreCodecQueueInputFormatDetails(
    std::shared_ptr<const fuchsia_media::FormatDetails> per_stream_override_format_details) {
  // nothing to do here
}

void FakeCodecAdapter::CoreCodecQueueInputPacket(const CodecPacket* packet) {
  events_->onCoreCodecInputPacketDone(packet);
}

void FakeCodecAdapter::CoreCodecQueueInputEndOfStream() {
  // nothing to do here
}

void FakeCodecAdapter::CoreCodecStopStream() {
  // nothing to do here
}

void FakeCodecAdapter::CoreCodecAddBuffer(CodecPort port, const CodecBuffer* buffer) {
  if (on_add_buffer_) {
    on_add_buffer_(port, buffer);
  }
}

void FakeCodecAdapter::CoreCodecRemoveBuffer(CodecPort port, const CodecBuffer* buffer) {
  if (on_remove_buffer_) {
    on_remove_buffer_(port, buffer);
  }
}

void FakeCodecAdapter::CoreCodecConfigureBuffers(
    CodecPort port, const std::vector<std::unique_ptr<CodecPacket>>& packets) {
  // nothing to do here
}

void FakeCodecAdapter::CoreCodecRecycleOutputPacket(CodecPacket* packet) {
  if (on_recycle_output_packet_) {
    on_recycle_output_packet_(packet);
  }
  packet->SetBuffer(nullptr);
}

void FakeCodecAdapter::CoreCodecEnsureBuffersNotConfigured(CodecPort port) {
  if (on_ensure_buffers_not_configured_) {
    on_ensure_buffers_not_configured_(port);
  }
}

fuchsia_media::StreamOutputFormat FakeCodecAdapter::CoreCodecGetOutputFormat2(
    uint64_t stream_lifetime_ordinal, uint64_t new_output_format_details_version_ordinal) {
  fuchsia_media::StreamOutputFormat result;
  result.stream_lifetime_ordinal() = stream_lifetime_ordinal;
  auto& format_details = result.format_details().emplace();
  format_details.format_details_version_ordinal() = new_output_format_details_version_ordinal;
  format_details.mime_type() = kOutputMimeType;
  auto& image_format = format_details.image_format().emplace();
  image_format.pixel_format() = fuchsia_images2::PixelFormat::kR8G8B8A8;
  image_format.color_space() = fuchsia_images2::ColorSpace::kSrgb;
  image_format.size() = fuchsia_math::SizeU{kCodedWidth, kCodedHeight};
  image_format.bytes_per_row() = kBytesPerRow;
  image_format.display_rect() = fuchsia_math::RectU{0, 0, kDisplayWidth, kDisplayHeight};
  return result;
}

void FakeCodecAdapter::CoreCodecMidStreamOutputBufferReConfigPrepare() {
  if (on_mid_stream_output_buffer_re_config_prepare_) {
    on_mid_stream_output_buffer_re_config_prepare_();
  }
}

void FakeCodecAdapter::CoreCodecMidStreamOutputBufferReConfigFinish() {
  // nothing to do here
}

void FakeCodecAdapter::CoreCodecCloseBufferLifetimeOrdinal(CodecPort port,
                                                           uint64_t buffer_lifetime_ordinal) {
  if (on_close_buffer_lifetime_ordinal_) {
    on_close_buffer_lifetime_ordinal_(port, buffer_lifetime_ordinal);
  }
}

void FakeCodecAdapter::SetSupportsDynamicBuffers(bool supports) {
  supports_dynamic_buffers_ = supports;
}

void FakeCodecAdapter::SetIsCoreCodecRequiringOutputConfigForFormatDetection(bool require) {
  require_output_config_for_format_detection_ = require;
}

void FakeCodecAdapter::SetIsCoreCodecHwBased(CodecPort port, bool is_hw_based) {
  is_hw_based_[port] = is_hw_based;
}

uint64_t FakeCodecAdapter::IncrementConstraintsVersion(CodecPort port) {
  return ++constraints_version_[port];
}

void FakeCodecAdapter::SetBufferCollectionConstraints(
    CodecPort port, fuchsia_sysmem2::BufferCollectionConstraints constraints) {
  buffer_collection_constraints_[port] = std::move(constraints);
}

void FakeCodecAdapter::SetOnAddBuffer(fit::function<void(CodecPort, const CodecBuffer*)> hook) {
  on_add_buffer_ = std::move(hook);
}

void FakeCodecAdapter::SetOnRemoveBuffer(fit::function<void(CodecPort, const CodecBuffer*)> hook) {
  on_remove_buffer_ = std::move(hook);
}

void FakeCodecAdapter::SetOnRecycleOutputPacket(fit::function<void(CodecPacket*)> hook) {
  on_recycle_output_packet_ = std::move(hook);
}

void FakeCodecAdapter::SetOnStartStream(fit::function<void()> hook) {
  on_start_stream_ = std::move(hook);
}

void FakeCodecAdapter::SetOnMidStreamOutputBufferReConfigPrepare(fit::function<void()> hook) {
  on_mid_stream_output_buffer_re_config_prepare_ = std::move(hook);
}

void FakeCodecAdapter::SetOnEnsureBuffersNotConfigured(fit::function<void(CodecPort)> hook) {
  on_ensure_buffers_not_configured_ = std::move(hook);
}

void FakeCodecAdapter::SetOnCloseBufferLifetimeOrdinal(
    fit::function<void(CodecPort, uint64_t)> hook) {
  on_close_buffer_lifetime_ordinal_ = std::move(hook);
}
