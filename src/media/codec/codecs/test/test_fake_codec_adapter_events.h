// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_CODEC_CODECS_TEST_TEST_FAKE_CODEC_ADAPTER_EVENTS_H_
#define SRC_MEDIA_CODEC_CODECS_TEST_TEST_FAKE_CODEC_ADAPTER_EVENTS_H_

#include <fuchsia/media/cpp/fidl.h>
#include <lib/media/codec_impl/codec_adapter_events.h>

#include <condition_variable>
#include <cstdarg>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <mutex>
#include <string>

class FakeCodecAdapterEvents : public CodecAdapterEvents {
 public:
  void onCoreCodecFailCodec(const char* format, ...) override {
    std::lock_guard<std::mutex> lock(mu_);
    fail_codec_count_++;
    char buffer[256];
    va_list args;
    va_start(args, format);
    std::vsnprintf(buffer, sizeof(buffer), format, args);
    va_end(args);
    last_fail_message_ = buffer;
    cv_.notify_all();
  }

  void onCoreCodecFailStream(fuchsia::media::StreamError error) override {
    std::lock_guard<std::mutex> lock(mu_);
    fail_stream_count_++;
    cv_.notify_all();
  }
  void onCoreCodecResetStreamAfterCurrentFrame() override {}
  void onCoreCodecMidStreamOutputConstraintsChange(bool output_re_config_required) override {
    std::lock_guard<std::mutex> lock(mu_);
    output_constraints_change_count_++;
    cv_.notify_all();
  }
  void onCoreCodecOutputFormatChange() override {}
  void onCoreCodecInputPacketDone(const CodecPacket* packet) override {
    std::lock_guard<std::mutex> lock(mu_);
    input_packet_done_count_++;
    cv_.notify_all();
  }
  void onCoreCodecOutputPacket(CodecPacket* packet, bool error_detected_before,
                               bool error_detected_during) override {
    std::lock_guard<std::mutex> lock(mu_);
    output_packet_count_++;
    cv_.notify_all();
  }
  void onCoreCodecOutputTimestampHasNoOutput(uint64_t timestamp_ish) override {}
  void onCoreCodecOutputEndOfStream(bool error_detected_before) override {
    std::lock_guard<std::mutex> lock(mu_);
    output_eos_count_++;
    cv_.notify_all();
  }
  void onCoreCodecLogEvent(
      media_metrics::StreamProcessorEvents2MigratedMetricDimensionEvent event_code) override {}

  size_t fail_codec_count() const {
    std::lock_guard<std::mutex> lock(mu_);
    return fail_codec_count_;
  }
  size_t fail_stream_count() const {
    std::lock_guard<std::mutex> lock(mu_);
    return fail_stream_count_;
  }
  std::string last_fail_message() const {
    std::lock_guard<std::mutex> lock(mu_);
    return last_fail_message_;
  }
  size_t input_packet_done_count() const {
    std::lock_guard<std::mutex> lock(mu_);
    return input_packet_done_count_;
  }
  size_t output_packet_count() const {
    std::lock_guard<std::mutex> lock(mu_);
    return output_packet_count_;
  }
  size_t output_eos_count() const {
    std::lock_guard<std::mutex> lock(mu_);
    return output_eos_count_;
  }

  void WaitForOutputConstraintsChangeCount(size_t expected) {
    std::unique_lock<std::mutex> lock(mu_);
    cv_.wait(lock, [this, expected] {
      return output_constraints_change_count_ >= expected || fail_codec_count_ > 0;
    });
  }

  void WaitForInputPacketDoneCount(size_t expected, bool stop_on_fail_codec = true) {
    std::unique_lock<std::mutex> lock(mu_);
    cv_.wait(lock, [this, expected, stop_on_fail_codec] {
      return input_packet_done_count_ >= expected || (stop_on_fail_codec && fail_codec_count_ > 0);
    });
  }

  void WaitForOutputEosCount(size_t expected) {
    std::unique_lock<std::mutex> lock(mu_);
    cv_.wait(lock,
             [this, expected] { return output_eos_count_ >= expected || fail_codec_count_ > 0; });
  }

 private:
  mutable std::mutex mu_;
  std::condition_variable cv_;
  size_t fail_codec_count_ = 0;
  size_t fail_stream_count_ = 0;
  size_t output_constraints_change_count_ = 0;
  size_t input_packet_done_count_ = 0;
  size_t output_packet_count_ = 0;
  size_t output_eos_count_ = 0;
  std::string last_fail_message_;
};

#endif  // SRC_MEDIA_CODEC_CODECS_TEST_TEST_FAKE_CODEC_ADAPTER_EVENTS_H_
