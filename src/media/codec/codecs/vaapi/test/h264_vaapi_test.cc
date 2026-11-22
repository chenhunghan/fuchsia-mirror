// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.io/cpp/fidl.h>
#include <fidl/fuchsia.media/cpp/fidl.h>
#include <fidl/fuchsia.mediacodec/cpp/fidl.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fdio/directory.h>
#include <stdio.h>

#include <memory>
#include <mutex>
#include <thread>

#include <gtest/gtest.h>

#include "src/lib/files/file.h"
#include "src/media/codec/codecs/test/test_codec_packets.h"
#include "src/media/codec/codecs/vaapi/codec_adapter_vaapi_decoder.h"
#include "src/media/codec/codecs/vaapi/codec_runner_app.h"
#include "src/media/codec/codecs/vaapi/vaapi_utils.h"
#include "vaapi_stubs.h"

namespace {

constexpr uint32_t kBearVideoWidth = 320u;
constexpr uint32_t kBearVideoHeight = 192u;
constexpr uint32_t kBearUncompressedFrameBytes = kBearVideoWidth * kBearVideoHeight * 3 / 2;

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

    std::lock_guard<std::mutex> guard(lock_);
    fail_codec_count_++;
    cond_.notify_all();
  }

  void onCoreCodecFailStream(fuchsia_media::StreamError error) override {
    printf("Got onCoreCodecFailStream %d\n", static_cast<int>(error));
    fflush(stdout);

    std::lock_guard<std::mutex> guard(lock_);
    fail_stream_count_++;
    cond_.notify_all();
  }

  void onCoreCodecResetStreamAfterCurrentFrame() override {}

  void onCoreCodecMidStreamOutputConstraintsChange(bool output_re_config_required) override {
    {
      std::lock_guard lock(lock_);
      // Test a representative value.
      auto output_constraints_result =
          codec_adapter_->CoreCodecGetBufferCollectionConstraints3(CodecPort::kOutputPort);
      ZX_ASSERT(output_constraints_result.has_value());
      output_constraints_ = std::move(output_constraints_result->constraints);
      EXPECT_TRUE(*output_constraints_.buffer_memory_constraints()->cpu_domain_supported());
      EXPECT_EQ(kBearVideoWidth,
                output_constraints_.image_format_constraints()->at(0).required_min_size()->width());
      output_constraints_set_ = true;
      cond_.notify_all();
    }
    if (reconfigure_in_constraints_change_) {
      ReconfigureBuffers();
    }
  }

  void onCoreCodecOutputFormatChange() override {}

  void onCoreCodecInputPacketDone(const CodecPacket *packet) override {
    std::lock_guard lock(lock_);
    input_packets_done_.push_back(packet);
    cond_.notify_all();
  }

  void onCoreCodecOutputPacket(CodecPacket *packet, bool error_detected_before,
                               bool error_detected_during) override {
    // Run `ASSERT_TRUE` checks inside a lambda so an assertion failure returns only from the
    // lambda rather than skipping `output_packets_done_.push_back` / `cond_.notify_all()` below
    // (which would deadlock `WaitForOutputPacketCount` on the main test thread).
    [&]() {
      auto output_format = codec_adapter_->CoreCodecGetOutputFormat2(1u, 1u);
      ASSERT_TRUE(output_format.format_details().has_value());
      ASSERT_TRUE(output_format.format_details()->image_format().has_value());
      const auto &image_format = output_format.format_details()->image_format().value();
      ASSERT_TRUE(image_format.size().has_value());
      // Test a representative value.
      EXPECT_EQ(kBearVideoWidth, image_format.size()->width());
    }();

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

  void ReconfigureBuffers() {
    std::unique_lock<std::mutex> lock(lock_);
    EXPECT_TRUE(output_constraints_set_);
    // Wait for buffer initialization to complete to ensure all buffers are staged to be loaded.
    cond_.wait(lock, [&]() { return buffer_initialization_completed_; });

    // Set the codec output format to the linear format
    auto output_constraints_result =
        codec_adapter_->CoreCodecGetBufferCollectionConstraints3(CodecPort::kOutputPort);
    ASSERT_TRUE(output_constraints_result.has_value());
    auto &output_constraints = output_constraints_result->constraints;
    fuchsia_sysmem2::BufferCollectionInfo buffer_collection;
    buffer_collection.settings().emplace().image_format_constraints() =
        output_constraints.image_format_constraints()->at(0);
    buffer_collection.buffers().emplace(*output_constraints.min_buffer_count_for_camping());
    if (!buffer_collection.settings()
             ->image_format_constraints()
             ->pixel_format_modifier()
             .has_value()) {
      buffer_collection.settings()->image_format_constraints()->pixel_format_modifier() =
          fuchsia_images2::PixelFormatModifier::kLinear;
    }
    codec_adapter_->CoreCodecSetBufferCollectionInfo(CodecPort::kOutputPort, buffer_collection);
    codec_adapter_->CoreCodecMidStreamOutputBufferReConfigFinish();
  }

  void set_reconfigure_in_constraints_change(bool reconfig) {
    reconfigure_in_constraints_change_ = reconfig;
  }

  void WaitForOutputConstraintsSet() {
    std::unique_lock<std::mutex> lock(lock_);
    cond_.wait(lock, [&]() { return output_constraints_set_; });
  }

 private:
  CodecAdapter *codec_adapter_ = nullptr;
  uint64_t fail_codec_count_{};
  uint64_t fail_stream_count_{};

  std::mutex lock_;
  std::condition_variable cond_;

  std::vector<const CodecPacket *> input_packets_done_;
  std::vector<CodecPacket *> output_packets_done_;
  bool buffer_initialization_completed_ = false;
  bool reconfigure_in_constraints_change_ = true;
  fuchsia_sysmem2::BufferCollectionConstraints output_constraints_;
  bool output_constraints_set_ = false;
};

class H264VaapiTestFixture : public ::testing::Test {
 protected:
  H264VaapiTestFixture() = default;
  ~H264VaapiTestFixture() override { decoder_.reset(); }

  void SetUp() override {
    EXPECT_TRUE(VADisplayWrapper::InitializeSingletonForTesting());

    vaDefaultStubSetReturn();

    // Have to defer the construction of decoder_ until
    // VADisplayWrapper::InitializeSingletonForTesting is called
    decoder_ = std::make_unique<CodecAdapterVaApiDecoder>(lock_, &events_);
    events_.set_codec_adapter(decoder_.get());
  }

  void TearDown() override { vaDefaultStubSetReturn(); }

  void CodecAndStreamInit() {
    fuchsia_media::FormatDetails format_details;
    format_details.format_details_version_ordinal(1);
    format_details.mime_type("video/h264");
    decoder_->CoreCodecInit(std::make_shared<const fuchsia_media::FormatDetails>(format_details));

    auto input_constraints_result =
        decoder_->CoreCodecGetBufferCollectionConstraints3(CodecPort::kInputPort);
    ASSERT_TRUE(input_constraints_result.has_value());
    auto &input_constraints = input_constraints_result->constraints;
    EXPECT_TRUE(*input_constraints.buffer_memory_constraints()->cpu_domain_supported());

    decoder_->CoreCodecStartStream();
    decoder_->CoreCodecQueueInputFormatDetails(
        std::make_shared<const fuchsia_media::FormatDetails>(format_details));
  }

  void CodecStreamStop() {
    decoder_->CoreCodecStopStream();
    decoder_->CoreCodecEnsureBuffersNotConfigured(CodecPort::kOutputPort);
  }

  void ParseFileIntoInputPackets(const std::string &file_name) {
    std::vector<uint8_t> result;
    ASSERT_TRUE(files::ReadFileToVector(file_name, &result));

    input_buffer_ = std::make_unique<CodecBufferForTest>(result.size(), 0, false);
    std::memcpy(input_buffer_->base(), result.data(), result.size());

    input_packet_ = std::make_unique<CodecPacketForTest>(0);
    input_packet_->SetStartOffset(0);
    input_packet_->SetValidLengthBytes(static_cast<uint32_t>(result.size()));
    input_packet_->SetBuffer(input_buffer_.get());
    decoder_->CoreCodecQueueInputPacket(input_packet_.get());
  }

  void ConfigureOutputBuffers(uint32_t output_packet_count, size_t output_packet_size) {
    auto test_packets = Packets(output_packet_count);
    test_buffers_ = Buffers(std::vector<size_t>(output_packet_count, output_packet_size));

    test_packets_ = std::vector<std::unique_ptr<CodecPacket>>(output_packet_count);
    for (size_t i = 0; i < output_packet_count; i++) {
      auto &packet = test_packets.packets[i];
      test_packets_[i] = std::move(packet);
      decoder_->CoreCodecAddBuffer(CodecPort::kOutputPort, test_buffers_.buffers[i].get());
    }

    decoder_->CoreCodecConfigureBuffers(CodecPort::kOutputPort, test_packets_);
    for (size_t i = 0; i < output_packet_count; i++) {
      decoder_->CoreCodecRecycleOutputPacket(test_packets_[i].get());
    }

    decoder_->CoreCodecConfigureBuffers(CodecPort::kOutputPort, test_packets_);
  }

  std::mutex lock_;
  FakeCodecAdapterEvents events_;
  std::unique_ptr<CodecAdapterVaApiDecoder> decoder_;
  std::unique_ptr<CodecPacketForTest> input_packet_;
  std::unique_ptr<CodecBufferForTest> input_buffer_;
  TestBuffers test_buffers_;
  std::vector<std::unique_ptr<CodecPacket>> test_packets_;
};

TEST_F(H264VaapiTestFixture, MimeTypeMismatchFailure) {
  constexpr uint64_t kExpectedNumOfCodecFailures = 1u;

  fuchsia_media::FormatDetails format_details;
  format_details.format_details_version_ordinal(1);
  format_details.mime_type("video/h264");
  decoder_->CoreCodecInit(std::make_shared<const fuchsia_media::FormatDetails>(format_details));
  decoder_->CoreCodecStartStream();

  fuchsia_media::FormatDetails format_details_mismatch;
  format_details_mismatch.format_details_version_ordinal(1);
  format_details_mismatch.mime_type("video/vp9");
  decoder_->CoreCodecQueueInputFormatDetails(
      std::make_shared<const fuchsia_media::FormatDetails>(format_details_mismatch));

  events_.SetBufferInitializationCompleted();
  events_.WaitForCodecFailure(kExpectedNumOfCodecFailures);

  CodecStreamStop();

  EXPECT_EQ(kExpectedNumOfCodecFailures, events_.fail_codec_count());
  EXPECT_EQ(0u, events_.fail_stream_count());
}

TEST_F(H264VaapiTestFixture, DecodeBasic) {
  constexpr uint32_t kExpectedOutputPackets = 29;

  CodecAndStreamInit();

  // Should be enough to handle a large fraction of bear.h264 output without recycling.
  constexpr uint32_t kOutputPacketCount = 35;
  constexpr size_t kOutputPacketSize = kBearUncompressedFrameBytes;

  ParseFileIntoInputPackets("/pkg/data/bear.h264");
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

// Check that delaying the output buffer configuration for a while doesn't cause a crash when
// outputting frames.
TEST_F(H264VaapiTestFixture, DelayedConfiguration) {
  constexpr uint32_t kExpectedOutputPackets = 29;

  events_.set_reconfigure_in_constraints_change(false);

  CodecAndStreamInit();

  // Should be enough to handle a large fraction of bear.h264 output without recycling.
  constexpr uint32_t kOutputPacketCount = 35;
  constexpr size_t kOutputPacketSize = kBearUncompressedFrameBytes;

  ParseFileIntoInputPackets("/pkg/data/bear.h264");

  sleep(1);

  events_.WaitForOutputConstraintsSet();
  ConfigureOutputBuffers(kOutputPacketCount, kOutputPacketSize);
  events_.SetBufferInitializationCompleted();
  events_.ReconfigureBuffers();
  events_.WaitForInputPacketsDone();
  events_.WaitForOutputPacketCount(kExpectedOutputPackets);
  events_.ReturnLastOutputPacket();

  CodecStreamStop();

  // One packet was returned, so it was already removed from the list.
  EXPECT_EQ(kExpectedOutputPackets - 1u, events_.output_packet_count());

  EXPECT_EQ(0u, events_.fail_codec_count());
  EXPECT_EQ(0u, events_.fail_stream_count());
}

TEST(H264Vaapi, CodecList) {
  EXPECT_TRUE(VADisplayWrapper::InitializeSingletonForTesting());
  auto codec_list = GetDeprecatedCodecList();
  // video/x-motion-jpeg decode, video/h264 decode, video/vp9 decode, video/h264 encode
  EXPECT_EQ(4u, codec_list.size());
}

TEST(H264Vaapi, CodecDescriptions) {
  EXPECT_TRUE(VADisplayWrapper::InitializeSingletonForTesting());
  auto codec_descriptions = GetCodecDescriptions();
  // video/x-motion-jpeg decode, video/h264 decode, video/vp9 decode, video/h264 encode

  // TODO(https://fxbug.dev/42166089): Uncomment this after new prebuilts have rolled.
  // EXPECT_EQ(4u, codec_descriptions.size());
}

// Test that we can connect using the CodecFactory.
TEST(H264Vaapi, Init) {
  EXPECT_TRUE(VADisplayWrapper::InitializeSingletonForTesting());
  auto [svc_client, svc_server] = fidl::Endpoints<fuchsia_io::Directory>::Create();
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);

  auto codec_factory_client_end =
      component::ConnectAt<fuchsia_mediacodec::CodecFactory>(svc_client);
  ASSERT_TRUE(codec_factory_client_end.is_ok());
  fidl::SyncClient<fuchsia_mediacodec::CodecFactory> codec_factory(
      std::move(*codec_factory_client_end));

  std::thread codec_thread([svc_server = std::move(svc_server)]() mutable {
    CodecRunnerApp<CodecAdapterVaApiDecoder, NoAdapter> runner_app;
    runner_app.Init();
    auto [outgoing_client, outgoing_server] = fidl::Endpoints<fuchsia_io::Directory>::Create();
    EXPECT_EQ(ZX_OK, runner_app.outgoing().Serve(std::move(outgoing_server)).status_value());
    EXPECT_EQ(ZX_OK, fdio_open3_at(outgoing_client.channel().get(), "svc",
                                   uint64_t{fuchsia_io::kPermReadable},
                                   svc_server.TakeChannel().release()));
    runner_app.Run();
  });

  auto [processor_client_end, processor_server_end] =
      fidl::Endpoints<fuchsia_media::StreamProcessor>::Create();

  fuchsia_mediacodec::CreateDecoderParams params;
  fuchsia_media::FormatDetails input_details;
  input_details.mime_type("video/h264");
  params.input_details(std::move(input_details));
  params.require_hw(true);
  EXPECT_TRUE(
      codec_factory->CreateDecoder({std::move(params), std::move(processor_server_end)}).is_ok());

  class EventHandler : public fidl::AsyncEventHandler<fuchsia_media::StreamProcessor> {
   public:
    explicit EventHandler(async::Loop &loop) : loop_(loop) {}

    void OnInputConstraints(
        fidl::Event<fuchsia_media::StreamProcessor::OnInputConstraints> &event) override {
      loop_.Quit();
    }

    void on_fidl_error(fidl::UnbindInfo error) override {
      loop_.Quit();
      ADD_FAILURE() << error;
    }

    void handle_unknown_event(
        fidl::UnknownEventMetadata<fuchsia_media::StreamProcessor> metadata) override {}

   private:
    async::Loop &loop_;
  };

  EventHandler event_handler(loop);
  fidl::Client<fuchsia_media::StreamProcessor> stream_processor(std::move(processor_client_end),
                                                                loop.dispatcher(), &event_handler);

  loop.Run();
  stream_processor = {};
  codec_factory = {};

  codec_thread.join();
}

}  // namespace
