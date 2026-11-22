// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.media/cpp/hlcpp_conversion.h>
#include <fidl/fuchsia.mediacodec/cpp/hlcpp_conversion.h>
#include <lib/media/codec_impl/codec_adapter.h>
#include <zircon/assert.h>

#include <limits>
#include <memory>

CodecAdapter::CodecAdapter(std::mutex& lock, CodecAdapterEvents* codec_adapter_events)
    : lock_(lock),
      events_(codec_adapter_events),
      random_device_(),
      not_for_security_prng_(random_device_()) {
  ZX_DEBUG_ASSERT(events_);
  // nothing else to do here
}

CodecAdapter::~CodecAdapter() {
  // nothing to do here
}

void CodecAdapter::SetCodecDiagnostics(CodecDiagnostics* codec_diagnostics) {
  // Default implementation does nothing with diagnostic data
}

std::optional<media_metrics::StreamProcessorEvents2MigratedMetricDimensionImplementation>
CodecAdapter::CoreCodecMetricsImplementation() {
  // This will cause a ZX_PANIC() if LogEvent() is being used by a sub-class, in which case the
  // sub-class must override CoreCodecMetricsImplementation().
  return std::nullopt;
}

void CodecAdapter::CoreCodecInit(
    std::shared_ptr<const fuchsia_media::FormatDetails> initial_input_format_details) {
  CoreCodecInit(fidl::NaturalToHLCPP(fuchsia_media::FormatDetails(*initial_input_format_details)));
}

void CodecAdapter::CoreCodecInit(
    const fuchsia::media::FormatDetails& initial_input_format_details) {
  ZX_PANIC("CoreCodecInit not implemented");
}

void CodecAdapter::CoreCodecSetSecureMemoryMode(
    CodecPort port, fuchsia_mediacodec::SecureMemoryMode secure_memory_mode) {
  CoreCodecSetSecureMemoryMode(port, fidl::NaturalToHLCPP(secure_memory_mode));
}

void CodecAdapter::CoreCodecSetSecureMemoryMode(
    CodecPort port, fuchsia::mediacodec::SecureMemoryMode secure_memory_mode) {
  if (secure_memory_mode != fuchsia::mediacodec::SecureMemoryMode::OFF) {
    events_->onCoreCodecFailCodec(
        "In CodecAdapter::CoreCodecSetSecureMemoryMode(), secure_memory_mode != OFF");
    return;
  }
  // CodecImpl will enforce that BufferCollection constraints and BufferCollectionInfo_2 are
  // consistent with OFF.
  return;
}

void CodecAdapter::CoreCodecQueueInputFormatDetails(
    std::shared_ptr<const fuchsia_media::FormatDetails> per_stream_override_format_details) {
  CoreCodecQueueInputFormatDetails(
      fidl::NaturalToHLCPP(fuchsia_media::FormatDetails(*per_stream_override_format_details)));
}

void CodecAdapter::CoreCodecQueueInputFormatDetails(
    const fuchsia::media::FormatDetails& per_stream_override_format_details) {
  ZX_PANIC("CoreCodecQueueInputFormatDetails not implemented");
}

std::unique_ptr<const fuchsia::media::StreamBufferConstraints>
CodecAdapter::CoreCodecBuildNewInputConstraints() {
  ZX_PANIC("never called");
}

std::unique_ptr<const fuchsia::media::StreamOutputConstraints>
CodecAdapter::CoreCodecBuildNewOutputConstraints(
    uint64_t stream_lifetime_ordinal, uint64_t new_output_buffer_constraints_version_ordinal,
    bool buffer_constraints_action_required) {
  ZX_PANIC("never called");
}

fuchsia_media::StreamOutputFormat CodecAdapter::CoreCodecGetOutputFormat2(
    uint64_t stream_lifetime_ordinal, uint64_t new_output_format_details_version_ordinal) {
  return fidl::HLCPPToNatural(
      CoreCodecGetOutputFormat(stream_lifetime_ordinal, new_output_format_details_version_ordinal));
}

fuchsia::media::StreamOutputFormat CodecAdapter::CoreCodecGetOutputFormat(
    uint64_t stream_lifetime_ordinal, uint64_t new_output_format_details_version_ordinal) {
  ZX_PANIC("CoreCodecGetOutputFormat not implemented");
}

void CodecAdapter::CoreCodecResetStreamAfterCurrentFrame() {
  ZX_PANIC(
      "onCoreCodecResetStreamAfterCurrentFrame() triggered by a CodecAdapter that doesn't override "
      "CoreCodecResetStreamAfterCurrentFrame()");
}
