// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.sysmem2/cpp/fidl.h>
#include <lib/fdio/directory.h>
#include <stdio.h>

#include <limits>
#include <memory>
#include <thread>

#include <gtest/gtest.h>

#include "src/lib/files/file.h"
#include "src/media/codec/codecs/test/test_codec_packets.h"
#include "src/media/codec/codecs/vaapi/codec_adapter_vaapi_encoder.h"
#include "src/media/codec/codecs/vaapi/codec_runner_app.h"
#include "src/media/codec/codecs/vaapi/third_party/chromium/h264_vaapi_video_encoder_delegate.h"
#include "src/media/codec/codecs/vaapi/third_party/chromium/vaapi_wrapper.h"
#include "src/media/codec/codecs/vaapi/vaapi_utils.h"
#include "src/media/third_party/chromium_media/geometry.h"
#include "src/media/third_party/chromium_media/media/gpu/gpu_video_encode_accelerator_helpers.h"
#include "src/media/third_party/chromium_media/media/parsers/h264_parser.h"
#include "vaapi_stubs.h"

namespace {

class FakeCodecAdapterEvents : public CodecAdapterEvents {
 public:
  void onCoreCodecFailCodec(const char *format, ...) override {
    va_list args;
    va_start(args, format);
    printf("Got onCoreCodecFailCodec: ");
    vprintf(format, args);
    printf("\n");
    fflush(stdout);
    va_end(args);

    fail_codec_count_++;
    cond_.notify_all();
  }

  void onCoreCodecFailStream(fuchsia::media::StreamError error) override {
    printf("Got onCoreCodecFailStream %d\n", static_cast<int>(error));
    fflush(stdout);
    fail_stream_count_++;
  }

  void onCoreCodecResetStreamAfterCurrentFrame() override {}

  void onCoreCodecMidStreamOutputConstraintsChange(bool output_re_config_required) override {
    // Test a representative value.
    auto output_constraints = codec_adapter_->CoreCodecGetBufferCollectionConstraints2(
        CodecPort::kOutputPort, fuchsia::media::StreamBufferConstraints(),
        fuchsia::media::StreamBufferPartialSettings());
    EXPECT_TRUE(*output_constraints.buffer_memory_constraints()->cpu_domain_supported());

    std::unique_lock<std::mutex> lock(lock_);
    mid_stream_output_constraints_change_count_++;
    // Wait for buffer initialization to complete to ensure all buffers are staged to be loaded.
    cond_.wait(lock, [&]() { return buffer_initialization_completed_; });

    // Fake out the client setting buffer constraints on sysmem
    fuchsia_sysmem2::BufferCollectionInfo buffer_collection;
    buffer_collection.settings().emplace();
    if (output_constraints.image_format_constraints().has_value()) {
      buffer_collection.settings()->image_format_constraints() =
          output_constraints.image_format_constraints()->at(0);
    }
    buffer_collection.buffers().emplace(*output_constraints.min_buffer_count_for_camping());
    codec_adapter_->CoreCodecSetBufferCollectionInfo(CodecPort::kOutputPort, buffer_collection);
    codec_adapter_->CoreCodecMidStreamOutputBufferReConfigFinish();
  }

  void onCoreCodecOutputFormatChange() override {}

  void onCoreCodecInputPacketDone(const CodecPacket *packet) override {
    std::lock_guard lock(lock_);
    input_packets_done_.push_back(packet);
    cond_.notify_all();
  }

  void onCoreCodecOutputPacket(CodecPacket *packet, bool error_detected_before,
                               bool error_detected_during) override {
    auto output_format = codec_adapter_->CoreCodecGetOutputFormat(1u, 1u);
    // Test a representative value.
    EXPECT_TRUE(output_format.format_details().domain().video().is_compressed());

    std::lock_guard lock(lock_);
    output_packets_done_.push_back(packet);
    cond_.notify_all();
  }

  void onCoreCodecOutputTimestampHasNoOutput(uint64_t timestamp_ish) override {}

  void onCoreCodecOutputEndOfStream(bool error_detected_before) override {
    printf("Got onCoreCodecOutputEndOfStream\n");
    fflush(stdout);
  }

  void onCoreCodecLogEvent(
      media_metrics::StreamProcessorEvents2MigratedMetricDimensionEvent event_code) override {}

  uint64_t fail_codec_count() const { return fail_codec_count_; }
  uint64_t fail_stream_count() const { return fail_stream_count_; }
  uint64_t mid_stream_output_constraints_change_count() const {
    return mid_stream_output_constraints_change_count_;
  }

  void WaitForInputPacketsDone() {
    std::unique_lock<std::mutex> lock(lock_);
    cond_.wait(lock, [this]() { return !input_packets_done_.empty(); });
  }

  void set_codec_adapter(CodecAdapter *codec_adapter) { codec_adapter_ = codec_adapter; }

  void WaitForOutputPacketCount(size_t output_packet_count) {
    std::unique_lock<std::mutex> lock(lock_);
    cond_.wait(lock, [&]() { return output_packets_done_.size() == output_packet_count; });
  }

  size_t output_packet_count() const { return output_packets_done_.size(); }

  void SetBufferInitializationCompleted() {
    std::lock_guard lock(lock_);
    buffer_initialization_completed_ = true;
    cond_.notify_all();
  }

  void WaitForCodecFailure(uint64_t failure_count) {
    std::unique_lock<std::mutex> lock(lock_);
    cond_.wait(lock, [&]() { return fail_codec_count_ == failure_count; });
  }

  void ReturnLastOutputPacket() {
    std::lock_guard lock(lock_);
    auto packet = output_packets_done_.back();
    output_packets_done_.pop_back();
    codec_adapter_->CoreCodecRecycleOutputPacket(packet);
  }

 private:
  CodecAdapter *codec_adapter_ = nullptr;
  uint64_t fail_codec_count_{};
  uint64_t fail_stream_count_{};
  uint64_t mid_stream_output_constraints_change_count_{};

  std::mutex lock_;
  std::condition_variable cond_;

  std::vector<const CodecPacket *> input_packets_done_;
  std::vector<CodecPacket *> output_packets_done_;
  bool buffer_initialization_completed_ = false;
};

class H264EncoderTestFixture : public ::testing::Test {
 protected:
  H264EncoderTestFixture() = default;
  ~H264EncoderTestFixture() override { encoder_.reset(); }

  void SetUp() override {
    EXPECT_TRUE(VADisplayWrapper::InitializeSingletonForTesting());

    vaDefaultStubSetReturn();

    // Have to defer the construction of encoder_ until
    // VADisplayWrapper::InitializeSingletonForTesting is called
    encoder_ = std::make_unique<CodecAdapterVaApiEncoder>(lock_, &events_);
    events_.set_codec_adapter(encoder_.get());
  }

  void TearDown() override { vaDefaultStubSetReturn(); }

  void CodecAndStreamInit() {
    fuchsia::media::FormatDetails format_details;
    format_details.set_format_details_version_ordinal(1);
    format_details.set_mime_type("video/h264");

    fuchsia::media::DomainFormat domain_format;
    domain_format.video().uncompressed().image_format.display_width = 10;
    domain_format.video().uncompressed().image_format.display_height = 10;
    domain_format.video().uncompressed().image_format.coded_width = 10;
    domain_format.video().uncompressed().image_format.coded_height = 10;
    format_details.set_domain(std::move(domain_format));
    encoder_->CoreCodecInit(format_details);

    auto input_constraints = encoder_->CoreCodecGetBufferCollectionConstraints2(
        CodecPort::kInputPort, fuchsia::media::StreamBufferConstraints(),
        fuchsia::media::StreamBufferPartialSettings());
    EXPECT_TRUE(*input_constraints.buffer_memory_constraints()->cpu_domain_supported());

    encoder_->CoreCodecStartStream();
    encoder_->CoreCodecQueueInputFormatDetails(format_details);
  }

  void CodecStreamStop() {
    encoder_->CoreCodecStopStream();
    encoder_->CoreCodecEnsureBuffersNotConfigured(CodecPort::kOutputPort);
  }

  void ConfigureOutputBuffers(uint32_t output_packet_count, size_t output_packet_size) {
    auto test_packets = Packets(output_packet_count);
    test_buffers_ = Buffers(std::vector<size_t>(output_packet_count, output_packet_size));

    test_packets_ = std::vector<std::unique_ptr<CodecPacket>>(output_packet_count);
    for (size_t i = 0; i < output_packet_count; i++) {
      auto &packet = test_packets.packets[i];
      test_packets_[i] = std::move(packet);
      encoder_->CoreCodecAddBuffer(CodecPort::kOutputPort, test_buffers_.buffers[i].get());
    }

    encoder_->CoreCodecConfigureBuffers(CodecPort::kOutputPort, test_packets_);
    for (size_t i = 0; i < output_packet_count; i++) {
      encoder_->CoreCodecRecycleOutputPacket(test_packets_[i].get());
    }

    encoder_->CoreCodecConfigureBuffers(CodecPort::kOutputPort, test_packets_);
  }

  std::mutex lock_;
  FakeCodecAdapterEvents events_;
  std::unique_ptr<CodecAdapterVaApiEncoder> encoder_;
  std::unique_ptr<CodecPacketForTest> input_packet_;
  std::unique_ptr<CodecBufferForTest> input_buffer_;
  TestBuffers test_buffers_;
  std::vector<std::unique_ptr<CodecPacket>> test_packets_;
};

TEST_F(H264EncoderTestFixture, InvalidFormat) {
  constexpr uint64_t kExpectedNumOfCodecFailures = 1u;

  fuchsia::media::FormatDetails format_details;
  format_details.set_format_details_version_ordinal(1);
  format_details.set_mime_type("video/h264");
  encoder_->CoreCodecInit(format_details);
  events_.WaitForCodecFailure(kExpectedNumOfCodecFailures);

  EXPECT_EQ(kExpectedNumOfCodecFailures, events_.fail_codec_count());
  EXPECT_EQ(0u, events_.fail_stream_count());
}

TEST_F(H264EncoderTestFixture, Resize) {
  constexpr uint32_t kExpectedOutputPackets = 2;

  CodecAndStreamInit();

  // Should be enough to handle a large fraction of bear.h264 output without recycling.
  constexpr uint32_t kOutputPacketCount = 35;
  // Nothing writes to the output packet so its size doesn't matter.
  constexpr size_t kOutputPacketSize = 4096;
  {
    auto input_constraints = encoder_->CoreCodecGetBufferCollectionConstraints2(
        CodecPort::kInputPort, fuchsia::media::StreamBufferConstraints(),
        fuchsia::media::StreamBufferPartialSettings());
    EXPECT_TRUE(*input_constraints.buffer_memory_constraints()->cpu_domain_supported());

    // Fake out the client setting buffer constraints on sysmem
    fuchsia_sysmem2::BufferCollectionInfo buffer_collection;
    buffer_collection.settings().emplace().image_format_constraints() =
        input_constraints.image_format_constraints()->at(0);
    encoder_->CoreCodecSetBufferCollectionInfo(CodecPort::kInputPort, buffer_collection);
  }

  constexpr uint32_t kInputStride = 16;
  constexpr uint32_t kInputBufferSize = kInputStride * 12 * 3 / 2;

  input_buffer_ = std::make_unique<CodecBufferForTest>(kInputBufferSize, 0, false);

  std::vector<std::unique_ptr<CodecPacketForTest>> input_packets;
  {
    auto input_packet = std::make_unique<CodecPacketForTest>(0);
    input_packet->SetStartOffset(0);
    input_packet->SetValidLengthBytes(kInputBufferSize);
    input_packet->SetBuffer(input_buffer_.get());
    encoder_->CoreCodecQueueInputPacket(input_packet.get());
    input_packets.push_back(std::move(input_packet));
  }
  {
    fuchsia::media::FormatDetails format_details;
    format_details.set_format_details_version_ordinal(2);
    format_details.set_mime_type("video/h264");

    fuchsia::media::DomainFormat domain_format;
    domain_format.video().uncompressed().image_format.display_width = 12;
    domain_format.video().uncompressed().image_format.display_height = 10;
    domain_format.video().uncompressed().image_format.coded_width = 12;
    domain_format.video().uncompressed().image_format.coded_height = 10;
    format_details.set_domain(std::move(domain_format));
    encoder_->CoreCodecQueueInputFormatDetails(format_details);
  }
  {
    auto input_packet = std::make_unique<CodecPacketForTest>(0);
    input_packet->SetStartOffset(0);
    input_packet->SetValidLengthBytes(kInputBufferSize);
    input_packet->SetBuffer(input_buffer_.get());
    encoder_->CoreCodecQueueInputPacket(input_packet.get());
    input_packets.push_back(std::move(input_packet));
  }
  ConfigureOutputBuffers(kOutputPacketCount, kOutputPacketSize);

  events_.SetBufferInitializationCompleted();
  events_.WaitForInputPacketsDone();
  events_.WaitForOutputPacketCount(kExpectedOutputPackets);
  events_.ReturnLastOutputPacket();

  CodecStreamStop();

  // One packet was returned, so it was already removed from the list.
  EXPECT_EQ(kExpectedOutputPackets - 1u, events_.output_packet_count());

  EXPECT_EQ(0u, events_.fail_codec_count());
  EXPECT_EQ(0u, events_.fail_stream_count());
}

TEST_F(H264EncoderTestFixture, EncodeBasic) {
  constexpr uint32_t kExpectedOutputPackets = 29;

  CodecAndStreamInit();

  // Should be enough to handle a large fraction of bear.h264 output without recycling.
  constexpr uint32_t kOutputPacketCount = 35;
  // Nothing writes to the output packet so its size doesn't matter.
  constexpr size_t kOutputPacketSize = 4096;
  {
    auto input_constraints = encoder_->CoreCodecGetBufferCollectionConstraints2(
        CodecPort::kInputPort, fuchsia::media::StreamBufferConstraints(),
        fuchsia::media::StreamBufferPartialSettings());
    EXPECT_TRUE(*input_constraints.buffer_memory_constraints()->cpu_domain_supported());

    // Fake out the client setting buffer constraints on sysmem
    fuchsia_sysmem2::BufferCollectionInfo buffer_collection;
    buffer_collection.settings().emplace().image_format_constraints() =
        input_constraints.image_format_constraints()->at(0);
    encoder_->CoreCodecSetBufferCollectionInfo(CodecPort::kInputPort, buffer_collection);
  }

  constexpr uint32_t kInputStride = 16;
  constexpr uint32_t kInputBufferSize = kInputStride * 10 * 3 / 2;

  input_buffer_ = std::make_unique<CodecBufferForTest>(kInputBufferSize, 0, false);

  std::vector<std::unique_ptr<CodecPacketForTest>> input_packets;
  for (size_t i = 0; i < 29; i++) {
    auto input_packet = std::make_unique<CodecPacketForTest>(0);
    input_packet->SetStartOffset(0);
    input_packet->SetValidLengthBytes(kInputBufferSize);
    input_packet->SetBuffer(input_buffer_.get());
    encoder_->CoreCodecQueueInputPacket(input_packet.get());
    input_packets.push_back(std::move(input_packet));
  }
  ConfigureOutputBuffers(kOutputPacketCount, kOutputPacketSize);

  events_.SetBufferInitializationCompleted();
  events_.WaitForInputPacketsDone();
  events_.WaitForOutputPacketCount(kExpectedOutputPackets);
  events_.ReturnLastOutputPacket();

  CodecStreamStop();

  // One packet was returned, so it was already removed from the list.
  EXPECT_EQ(kExpectedOutputPackets - 1u, events_.output_packet_count());

  EXPECT_EQ(0u, events_.fail_codec_count());
  EXPECT_EQ(0u, events_.fail_stream_count());
}

// Test that we can connect using the CodecFactory.
TEST(H264Encoder, Init) {
  EXPECT_TRUE(VADisplayWrapper::InitializeSingletonForTesting());
  fidl::InterfaceRequest<fuchsia::io::Directory> directory_request;
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);

  auto codec_services = sys::ServiceDirectory::CreateWithRequest(&directory_request);

  std::thread codec_thread([directory_request = std::move(directory_request)]() mutable {
    CodecRunnerApp<NoAdapter, CodecAdapterVaApiEncoder> runner_app;
    runner_app.Init();
    fidl::InterfaceHandle<fuchsia::io::Directory> outgoing_directory;
    EXPECT_EQ(ZX_OK,
              runner_app.component_context()->outgoing()->Serve(outgoing_directory.NewRequest()));
    EXPECT_EQ(ZX_OK, fdio_open3_at(outgoing_directory.channel().get(), "svc",
                                   uint64_t{fuchsia::io::PERM_READABLE},
                                   directory_request.TakeChannel().release()));
    runner_app.Run();
  });

  fuchsia::mediacodec::CodecFactorySyncPtr codec_factory;
  codec_services->Connect(codec_factory.NewRequest());
  fuchsia::media::StreamProcessorPtr stream_processor;
  fuchsia::mediacodec::CreateEncoder_Params params;
  fuchsia::media::FormatDetails input_details;
  input_details.set_mime_type("video/h264");
  input_details.set_format_details_version_ordinal(1);

  fuchsia::media::DomainFormat domain_format;
  domain_format.video().uncompressed().image_format.display_width = 10;
  domain_format.video().uncompressed().image_format.display_height = 10;
  input_details.set_domain(std::move(domain_format));
  params.set_input_details(std::move(input_details));
  params.set_require_hw(true);
  EXPECT_EQ(ZX_OK, codec_factory->CreateEncoder(std::move(params), stream_processor.NewRequest()));

  stream_processor.set_error_handler([&](zx_status_t status) {
    loop.Quit();
    EXPECT_TRUE(false);
  });

  stream_processor.events().OnInputConstraints =
      [&](fuchsia::media::StreamBufferConstraints constraints) {
        loop.Quit();
        stream_processor.Unbind();
      };

  loop.Run();
  codec_factory.Unbind();

  codec_thread.join();
}

TEST(H264Encoder, BitstreamBufferSizeNoOverflow) {
  constexpr size_t kExpected8MB = 8 * 1024 * 1024;
  constexpr size_t kExpected4MB = 4 * 1024 * 1024;
  constexpr size_t kExpected2MB = 2 * 1024 * 1024;

  // 1. 65536 * 65536 overflows a 32-bit signed int to 0. Verify that buffer
  // sizing uses 64-bit area and selects the 8 MB max buffer size (> 1440p)
  // rather than falling back to 2 MB (or matching the 320x180 table entry).
  const gfx::Size large_size(65536, 65536);
  EXPECT_EQ(kExpected8MB, media::GetEncodeBitstreamBufferSize(large_size));
  EXPECT_EQ(kExpected8MB, media::GetEncodeBitstreamBufferSize(large_size, 20000000u, 30u));

  // 2. 46341 * 46341 overflows a 32-bit signed int to a negative value
  // (-2,147,479,015). Verify that 64-bit area prevents matching the first
  // table entry (<= 320 * 180) and selects the 8 MB max buffer size.
  const gfx::Size negative_overflow_size(46341, 46341);
  EXPECT_EQ(kExpected8MB, media::GetEncodeBitstreamBufferSize(negative_overflow_size));
  EXPECT_EQ(kExpected8MB,
            media::GetEncodeBitstreamBufferSize(negative_overflow_size, 100000u, 30u));

  // 3. Exercise the base::saturated_cast<size_t>(data.buffer_size_in_bytes * ratio)
  // clamping path inside the table loop using a table-matching size (1080p) and
  // an extreme bitrate/framerate ratio.
  const gfx::Size size_1080p(1920, 1080);
  EXPECT_EQ(kExpected2MB, media::GetEncodeBitstreamBufferSize(
                              size_1080p, std::numeric_limits<uint32_t>::max(), 1u));

  // 4. Verify exact resolution tier transitions in GetMaxEncodeBitstreamBufferSize():
  // <= 1080p -> 2 MB, (1080p, 1440p] -> 4 MB, > 1440p -> 8 MB.
  EXPECT_EQ(kExpected2MB, media::GetEncodeBitstreamBufferSize(gfx::Size(1920, 1080)));
  EXPECT_EQ(kExpected4MB, media::GetEncodeBitstreamBufferSize(gfx::Size(1920, 1081)));
  EXPECT_EQ(kExpected4MB, media::GetEncodeBitstreamBufferSize(gfx::Size(2560, 1440)));
  EXPECT_EQ(kExpected8MB, media::GetEncodeBitstreamBufferSize(gfx::Size(2560, 1441)));

  // 5. Verify that negative constructor arguments and setter inputs are clamped
  // to 0, preserving the non-negative invariant required by Area64() (which
  // casts directly to uint64_t without sign checks).
  gfx::Size clamped_size(-100, -200);
  EXPECT_EQ(0, clamped_size.width());
  EXPECT_EQ(0, clamped_size.height());
  EXPECT_EQ(0ULL, clamped_size.Area64());
  EXPECT_EQ(0, clamped_size.GetCheckedArea().ValueOrDie());

  clamped_size.set_width(-50);
  clamped_size.set_height(-75);
  EXPECT_EQ(0, clamped_size.width());
  EXPECT_EQ(0, clamped_size.height());
  EXPECT_EQ(0ULL, clamped_size.Area64());
}

TEST(H264Encoder, GeometrySafeMathAndToString) {
  const gfx::Size overflowing_size(65536, 65536);
  EXPECT_FALSE(overflowing_size.GetCheckedArea().IsValid());
  EXPECT_EQ(4294967296ULL, overflowing_size.Area64());
  EXPECT_EQ("65536x65536", overflowing_size.ToString());

  const gfx::Size valid_size(1920, 1080);
  EXPECT_TRUE(valid_size.GetCheckedArea().IsValid());
  EXPECT_EQ(1920 * 1080, valid_size.GetCheckedArea().ValueOrDie());

  const gfx::Point pt(10, 20);
  EXPECT_EQ("10,20", pt.ToString());

  // Verify Rect preserves x, y, width, height and returns CheckedNumeric from
  // right() and bottom() that detects signed integer overflow via IsValid().
  const gfx::Rect overflowing_rect(std::numeric_limits<int>::max() - 10,
                                   std::numeric_limits<int>::max() - 20, 100, 200);
  EXPECT_EQ(100, overflowing_rect.width());
  EXPECT_EQ(200, overflowing_rect.height());
  EXPECT_FALSE(overflowing_rect.right().IsValid());
  EXPECT_FALSE(overflowing_rect.bottom().IsValid());
  EXPECT_FALSE(overflowing_rect.IsValid());

  const gfx::Rect valid_rect(10, 20, 100, 200);
  EXPECT_TRUE(valid_rect.right().IsValid());
  EXPECT_TRUE(valid_rect.bottom().IsValid());
  EXPECT_TRUE(valid_rect.IsValid());
  EXPECT_EQ(110, valid_rect.right().ValueOrDie());
  EXPECT_EQ(220, valid_rect.bottom().ValueOrDie());
  EXPECT_TRUE(valid_rect.Contains(50, 50));
  EXPECT_FALSE(valid_rect.Contains(200, 200));
  EXPECT_TRUE(valid_rect.Contains(gfx::Rect(20, 30, 50, 50)));
  EXPECT_FALSE(valid_rect.Contains(gfx::Rect(20, 30, 100, 200)));
}

TEST(H264Encoder, H264SPSGetVisibleRectRejectsOverflowingRect) {
  media::H264SPS sps;
  sps.frame_mbs_only_flag = true;
  sps.chroma_format_idc = 1;  // 4:2:0 -> crop_unit_x = 2, crop_unit_y = 2.
  sps.chroma_array_type = 1;

  // 1. Valid 1080p SPS with bottom cropping (1920x1088 cropped by 8 rows to 1920x1080).
  sps.pic_width_in_mbs_minus1 = 119;        // 120 * 16 = 1920
  sps.pic_height_in_map_units_minus1 = 67;  // 68 * 16 = 1088
  sps.frame_cropping_flag = true;
  sps.frame_crop_left_offset = 0;
  sps.frame_crop_right_offset = 0;
  sps.frame_crop_top_offset = 0;
  sps.frame_crop_bottom_offset = 4;  // crop_bottom = 8
  auto valid_visible_rect = sps.GetVisibleRect();
  ASSERT_TRUE(valid_visible_rect.has_value());
  EXPECT_EQ(gfx::Rect(0, 0, 1920, 1080), valid_visible_rect.value());

  // 2. Cropping offsets that cause visible_rect.right() (x + width) to overflow
  // signed 32-bit int without triggering UB during width computation:
  // coded_width = 16 * (134217726 + 1) = 2147483632 (INT_MAX - 15).
  // crop_left = 100, crop_right = -100 -> visible_width = 2147483632 (fits in int).
  // However, visible_rect.right() = 100 + 2147483632 = 2147483732 > INT_MAX,
  // which is detected by visible_rect.IsValid() and rejected cleanly.
  sps.pic_width_in_mbs_minus1 = std::numeric_limits<int>::max() / 16 - 1;
  sps.frame_crop_left_offset = 50;    // crop_left = 100
  sps.frame_crop_right_offset = -50;  // crop_right = -100
  EXPECT_FALSE(sps.GetVisibleRect().has_value());
}

TEST_F(H264EncoderTestFixture, RejectOverflowingOrExcessiveCodedSize) {
  // Case 1: valid display_size (1920x1080) with 32-bit area-overflowing coded_size (65536x65536).
  {
    fuchsia::media::FormatDetails format_details;
    format_details.set_format_details_version_ordinal(1);
    format_details.set_mime_type("video/h264");

    fuchsia::media::DomainFormat domain_format;
    domain_format.video().uncompressed().image_format.display_width = 1920;
    domain_format.video().uncompressed().image_format.display_height = 1080;
    domain_format.video().uncompressed().image_format.coded_width = 65536;
    domain_format.video().uncompressed().image_format.coded_height = 65536;
    format_details.set_domain(std::move(domain_format));
    encoder_->CoreCodecInit(format_details);
    events_.WaitForCodecFailure(1u);
    EXPECT_EQ(1u, events_.fail_codec_count());
  }

  // Case 2: coded_size (4096x2160) doesn't overflow int, but exceeds kMaxInputWidth (3840).
  {
    fuchsia::media::FormatDetails format_details;
    format_details.set_format_details_version_ordinal(1);
    format_details.set_mime_type("video/h264");

    fuchsia::media::DomainFormat domain_format;
    domain_format.video().uncompressed().image_format.display_width = 1920;
    domain_format.video().uncompressed().image_format.display_height = 1080;
    domain_format.video().uncompressed().image_format.coded_width = 4096;
    domain_format.video().uncompressed().image_format.coded_height = 2160;
    format_details.set_domain(std::move(domain_format));
    encoder_->CoreCodecInit(format_details);
    events_.WaitForCodecFailure(2u);
    EXPECT_EQ(2u, events_.fail_codec_count());
  }

  // Case 3: coded_size (3000x3000) has width and height <= 3840 individually,
  // but pixel area (9,000,000) exceeds kMaxInputArea (3840 * 2160 = 8,294,400).
  {
    fuchsia::media::FormatDetails format_details;
    format_details.set_format_details_version_ordinal(1);
    format_details.set_mime_type("video/h264");

    fuchsia::media::DomainFormat domain_format;
    domain_format.video().uncompressed().image_format.display_width = 1920;
    domain_format.video().uncompressed().image_format.display_height = 1080;
    domain_format.video().uncompressed().image_format.coded_width = 3000;
    domain_format.video().uncompressed().image_format.coded_height = 3000;
    format_details.set_domain(std::move(domain_format));
    encoder_->CoreCodecInit(format_details);
    events_.WaitForCodecFailure(3u);
    EXPECT_EQ(3u, events_.fail_codec_count());
  }
}

TEST_F(H264EncoderTestFixture, ResizeCodedSizeOnly) {
  constexpr uint32_t kExpectedOutputPackets = 2;

  CodecAndStreamInit();

  constexpr uint32_t kOutputPacketCount = 35;
  constexpr size_t kOutputPacketSize = 4096;
  {
    auto input_constraints = encoder_->CoreCodecGetBufferCollectionConstraints2(
        CodecPort::kInputPort, fuchsia::media::StreamBufferConstraints(),
        fuchsia::media::StreamBufferPartialSettings());
    EXPECT_TRUE(*input_constraints.buffer_memory_constraints()->cpu_domain_supported());

    fuchsia_sysmem2::BufferCollectionInfo buffer_collection;
    buffer_collection.settings().emplace().image_format_constraints() =
        input_constraints.image_format_constraints()->at(0);
    encoder_->CoreCodecSetBufferCollectionInfo(CodecPort::kInputPort, buffer_collection);
  }

  // Size buffer for the larger coded_size (16x20 -> stride 16 * height 20 * 3 / 2 = 480 bytes).
  constexpr uint32_t kInputStride = 16;
  constexpr uint32_t kInputBufferSize = kInputStride * 20 * 3 / 2;

  input_buffer_ = std::make_unique<CodecBufferForTest>(kInputBufferSize, 0, false);

  std::vector<std::unique_ptr<CodecPacketForTest>> input_packets;
  {
    auto input_packet = std::make_unique<CodecPacketForTest>(0);
    input_packet->SetStartOffset(0);
    input_packet->SetValidLengthBytes(kInputBufferSize);
    input_packet->SetBuffer(input_buffer_.get());
    encoder_->CoreCodecQueueInputPacket(input_packet.get());
    input_packets.push_back(std::move(input_packet));
  }
  {
    // Keep display_size unchanged (10x10) while changing coded_size (10x10 -> 16x20)
    // to verify that HandleInputFormatChange triggers reset_encoder when only coded_size changes.
    fuchsia::media::FormatDetails format_details;
    format_details.set_format_details_version_ordinal(2);
    format_details.set_mime_type("video/h264");

    fuchsia::media::DomainFormat domain_format;
    domain_format.video().uncompressed().image_format.display_width = 10;
    domain_format.video().uncompressed().image_format.display_height = 10;
    domain_format.video().uncompressed().image_format.coded_width = 16;
    domain_format.video().uncompressed().image_format.coded_height = 20;
    format_details.set_domain(std::move(domain_format));
    encoder_->CoreCodecQueueInputFormatDetails(format_details);
  }
  {
    auto input_packet = std::make_unique<CodecPacketForTest>(0);
    input_packet->SetStartOffset(0);
    input_packet->SetValidLengthBytes(kInputBufferSize);
    input_packet->SetBuffer(input_buffer_.get());
    encoder_->CoreCodecQueueInputPacket(input_packet.get());
    input_packets.push_back(std::move(input_packet));
  }
  ConfigureOutputBuffers(kOutputPacketCount, kOutputPacketSize);

  events_.SetBufferInitializationCompleted();
  events_.WaitForInputPacketsDone();
  events_.WaitForOutputPacketCount(kExpectedOutputPackets);
  events_.ReturnLastOutputPacket();

  CodecStreamStop();

  // Verify that mid-stream output constraints change was triggered twice:
  // once on initial stream start and once after the coded_size-only format change.
  EXPECT_EQ(2u, events_.mid_stream_output_constraints_change_count());
  EXPECT_EQ(kExpectedOutputPackets - 1u, events_.output_packet_count());
  EXPECT_EQ(0u, events_.fail_codec_count());
  EXPECT_EQ(0u, events_.fail_stream_count());
}

TEST(H264Encoder, DelegateRejectsOverflowingVisibleSize) {
  EXPECT_TRUE(VADisplayWrapper::InitializeSingletonForTesting());
  auto vaapi_wrapper = std::make_shared<media::VaapiWrapper>();
  media::H264VaapiVideoEncoderDelegate delegate(vaapi_wrapper, fit::function<void()>());

  media::VideoEncodeAccelerator::Config config;
  // Even dimensions near INT_MAX that would overflow signed int when rounded up to 16.
  config.input_visible_size = gfx::Size(std::numeric_limits<int>::max() - 1, 1080);
  config.output_profile = media::H264PROFILE_HIGH;
  config.framerate = 30;
  config.bitrate = media::Bitrate::ConstantBitrate(200000u);

  media::VaapiVideoEncoderDelegate::Config ave_config;
  ave_config.max_num_ref_frames = 4;

  EXPECT_FALSE(delegate.Initialize(config, ave_config));

  // Verify that framerate values overflowing framesize_in_mbs * framerate (uint32_t)
  // or framerate * 2 (int) are rejected.
  config.input_visible_size = gfx::Size(1920, 1080);
  // 8160 MBs * 526323 = 4294795680 + 8160 > UINT32_MAX (wraps to 3264 if unchecked).
  config.framerate = 526323u;
  EXPECT_FALSE(delegate.Initialize(config, ave_config));
  // Small frame size (16x16 = 1 MB) where framesize_in_mbs * framerate fits in uint32_t,
  // but framerate * 2 overflows signed 32-bit int time_scale.
  config.input_visible_size = gfx::Size(16, 16);
  config.framerate = static_cast<uint32_t>(std::numeric_limits<int>::max()) / 2 + 1u;
  EXPECT_FALSE(delegate.Initialize(config, ave_config));

  // Initialize with valid parameters and verify dynamic UpdateRates() rejects
  // a framerate that overflows signed 32-bit int when multiplied by 2.
  config.input_visible_size = gfx::Size(1920, 1080);
  config.framerate = 30;
  ASSERT_TRUE(delegate.Initialize(config, ave_config));
  EXPECT_FALSE(
      delegate.UpdateRates(media::AllocateBitrateForDefaultEncoding(config),
                           static_cast<uint32_t>(std::numeric_limits<int>::max()) / 2 + 1u));
  EXPECT_TRUE(delegate.UpdateRates(media::AllocateBitrateForDefaultEncoding(config), 60u));

  // Also test CheckedRoundUp directly.
  EXPECT_EQ(16, CheckedRoundUp(1, 16).ValueOrDie());
  EXPECT_EQ(16, CheckedRoundUp(16, 16).ValueOrDie());
  EXPECT_EQ(0u, CheckedRoundUp(0u, 16u).ValueOrDie());
  EXPECT_FALSE(CheckedRoundUp(std::numeric_limits<int>::max() - 1, 16).IsValid());
  EXPECT_FALSE(CheckedRoundUp(10, 0).IsValid());
  EXPECT_FALSE(CheckedRoundUp(10, -16).IsValid());
  EXPECT_FALSE(CheckedRoundUp(-10, 16).IsValid());
  // Exact multiples near integer limits must not suffer false-positive overflow.
  EXPECT_EQ(0xFFFFFFF0u, CheckedRoundUp(0xFFFFFFF0u, 16u).ValueOrDie());
  EXPECT_EQ(2147483646, CheckedRoundUp(2147483646, 3).ValueOrDie());
}

TEST(H264Encoder, UploadVideoFrameToSurfaceBoundsCheck) {
  EXPECT_TRUE(VADisplayWrapper::InitializeSingletonForTesting());
  vaDefaultStubSetReturn();

  auto vaapi_wrapper = std::make_shared<media::VaapiWrapper>();
  VASurfaceID surface_id = 0;
  ASSERT_EQ(VA_STATUS_SUCCESS,
            vaCreateSurfaces(VADisplayWrapper::GetSingleton()->display(), VA_RT_FORMAT_YUV420, 16,
                             16, &surface_id, 1, nullptr, 0));
  ScopedSurfaceID scoped_surface(surface_id);

  // Buffer sized for 10x10 surface (240 bytes), but coded_size height is 20 so
  // UV plane starts at offset 20 * 16 = 320 bytes (requiring 480 bytes total).
  std::vector<uint8_t> small_buffer(240, 0);
  media::VideoFrame frame;
  frame.display_size = gfx::Size(10, 10);
  frame.coded_size = gfx::Size(16, 20);
  frame.stride = 16;
  frame.base = small_buffer.data();
  frame.size_bytes = small_buffer.size();

  EXPECT_FALSE(vaapi_wrapper->UploadVideoFrameToSurface(frame, surface_id, gfx::Size(10, 10)));

  // With sufficient buffer size for coded_size (16 * 20 + 16 * 10 = 480 bytes), upload succeeds.
  // Populate distinct byte patterns to verify UV plane bytes are copied from
  // coded_size.height() * stride (offset 320) rather than display_size.height() * stride (offset
  // 160).
  std::vector<uint8_t> valid_buffer(480, 0);
  std::fill(valid_buffer.begin(), valid_buffer.begin() + 160, 0xAA);
  std::fill(valid_buffer.begin() + 160, valid_buffer.begin() + 320, 0xDE);
  std::fill(valid_buffer.begin() + 320, valid_buffer.end(), 0x55);
  frame.base = valid_buffer.data();
  frame.size_bytes = valid_buffer.size();
  EXPECT_TRUE(vaapi_wrapper->UploadVideoFrameToSurface(frame, surface_id, gfx::Size(10, 10)));

  // Map back the destination surface and verify Y and UV plane pixel bytes.
  {
    VAImage image{};
    ASSERT_EQ(VA_STATUS_SUCCESS,
              vaDeriveImage(VADisplayWrapper::GetSingleton()->display(), surface_id, &image));
    ScopedImageID scoped_image(image.image_id);
    uint8_t *mapped_ptr = nullptr;
    ASSERT_EQ(VA_STATUS_SUCCESS, vaMapBuffer(VADisplayWrapper::GetSingleton()->display(), image.buf,
                                             reinterpret_cast<void **>(&mapped_ptr)));
    ASSERT_NE(nullptr, mapped_ptr);
    for (uint32_t y = 0; y < 10; ++y) {
      for (uint32_t x = 0; x < 10; ++x) {
        EXPECT_EQ(0xAA, mapped_ptr[image.offsets[0] + y * image.pitches[0] + x]);
      }
    }
    for (uint32_t y = 0; y < 5; ++y) {
      for (uint32_t x = 0; x < 10; ++x) {
        EXPECT_EQ(0x55, mapped_ptr[image.offsets[1] + y * image.pitches[1] + x]);
      }
    }
    EXPECT_EQ(VA_STATUS_SUCCESS,
              vaUnmapBuffer(VADisplayWrapper::GetSingleton()->display(), image.buf));
  }

  // Empty display_size is rejected.
  frame.display_size = gfx::Size(0, 10);
  EXPECT_FALSE(vaapi_wrapper->UploadVideoFrameToSurface(frame, surface_id, gfx::Size(10, 10)));

  // display_size exceeding coded_size is rejected.
  frame.display_size = gfx::Size(20, 10);
  EXPECT_FALSE(vaapi_wrapper->UploadVideoFrameToSurface(frame, surface_id, gfx::Size(10, 10)));

  // display_size exceeding input_surface_size is rejected.
  frame.display_size = gfx::Size(12, 12);
  EXPECT_FALSE(vaapi_wrapper->UploadVideoFrameToSurface(frame, surface_id, gfx::Size(10, 10)));

  // Restore valid display_size for remaining checks.
  frame.display_size = gfx::Size(10, 10);

  // Stride smaller than coded_size.width() is rejected.
  frame.stride = 12;
  EXPECT_FALSE(vaapi_wrapper->UploadVideoFrameToSurface(frame, surface_id, gfx::Size(10, 10)));

  // Destination VAImage smaller than display_size is rejected by VAImage bounds check.
  frame.stride = 16;
  VASurfaceID small_surface_id = 0;
  ASSERT_EQ(VA_STATUS_SUCCESS,
            vaCreateSurfaces(VADisplayWrapper::GetSingleton()->display(), VA_RT_FORMAT_YUV420, 8, 8,
                             &small_surface_id, 1, nullptr, 0));
  ScopedSurfaceID scoped_small_surface(small_surface_id);
  EXPECT_FALSE(
      vaapi_wrapper->UploadVideoFrameToSurface(frame, small_surface_id, gfx::Size(10, 10)));
}

}  // namespace
