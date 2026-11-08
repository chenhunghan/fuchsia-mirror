// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fake_codec_adapter.h>
#include <fuchsia/media/cpp/fidl.h>
#include <fuchsia/media/drm/cpp/fidl.h>
#include <fuchsia/mediacodec/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/media/codec_impl/fourcc.h>
#include <lib/zx/time.h>

#include <gtest/gtest.h>

#include "lib/media/codec_impl/codec_impl.h"
#include "src/lib/testing/loop_fixture/real_loop_fixture.h"

namespace {

auto CreateDecoderParams() {
  fuchsia::mediacodec::CreateDecoder_Params params;
  params.mutable_input_details()->set_format_details_version_ordinal(0).set_mime_type("video/vp9");
  return params;
}

auto CreateEncoderParams() {
  fuchsia::mediacodec::CreateEncoder_Params params;
  params.mutable_input_details()->set_format_details_version_ordinal(0).set_mime_type("audio/sbc");
  return params;
}

auto CreateDecryptorParams() {
  fuchsia::media::drm::DecryptorParams params;
  params.mutable_input_details()->set_format_details_version_ordinal(0);
  return params;
}

void ExpectFormatDetailsFidlValid(const fuchsia::media::FormatDetails& format_details) {
  fuchsia::media::FormatDetails copy = fidl::Clone(format_details);
  fidl::BodyEncoder encoder(fidl::internal::WireFormatVersion::kV2);
  size_t offset = encoder.Alloc(
      fidl::EncodingInlineSize<fuchsia::media::FormatDetails, fidl::Encoder>(&encoder));
  fidl::Encode(&encoder, &copy, offset);
  const char* error_msg = nullptr;
  EXPECT_EQ(ZX_OK, encoder.GetBody().Validate(fidl::internal::WireFormatVersion::kV2,
                                              fuchsia::media::FormatDetails::FidlType, &error_msg))
      << (error_msg ? error_msg : "");
}

}  // namespace

class CodecImplLifetime : public gtest::RealLoopFixture {
 protected:
  CodecImplLifetime()
      : loop_separate_thread_(&kAsyncLoopConfigNoAttachToCurrentThread),
        admission_control_(dispatcher()) {
    // nothing else to do here
  }

  ~CodecImplLifetime() {
    // Force to teardown before admission_control_.
    RunLoopUntilIdle();
    QuitLoop();
  }

  void SetUp() override { loop_separate_thread_.StartThread("separate_thread"); }
  void TearDown() override {
    // Force any failure during ~CodecImpl to have more obvious stack.
    codec_impl_ = nullptr;
  }

  void Create(bool bind = true, bool delete_async = false,
              CodecImpl::StreamProcessorParams params = CreateDecoderParams()) {
    // Just hold onto the server end and never connect it to anything, for now.
    auto sysmem_endpoints = fidl::CreateEndpoints<fuchsia_sysmem2::Allocator>();
    ZX_ASSERT(sysmem_endpoints.is_ok());
    sysmem_client_ = std::move(sysmem_endpoints->client);
    sysmem_request_ = std::move(sysmem_endpoints->server);
    codec_request_ = codec_client_handle_.NewRequest();
    ZX_DEBUG_ASSERT(!delete_async || bind);
    std::unique_ptr<CodecImpl> codec_impl;
    admission_control_.TryAddCodec(
        true, [this, bind, delete_async, params = std::move(params),
               &codec_impl](std::unique_ptr<CodecAdmission> codec_admission) mutable {
          codec_impl = std::make_unique<CodecImpl>(std::move(sysmem_client_),
                                                   std::move(codec_admission), dispatcher(),
                                                   std::move(params), std::move(codec_request_));
          auto fake_codec_adapter =
              std::make_unique<FakeCodecAdapter>(codec_impl->lock(), codec_impl.get());
          fake_codec_adapter_ = fake_codec_adapter.get();
          codec_impl->SetCoreCodecAdapter(std::move(fake_codec_adapter));
          if (!bind) {
            return;
          }
          codec_impl->BindAsync([this, delete_async] {
            if (!delete_async) {
              codec_impl_ = nullptr;
            } else {
              zx_status_t status = async::PostTask(dispatcher(), [this] { codec_impl_ = nullptr; });
              ZX_DEBUG_ASSERT(status == ZX_OK);
            }
            error_handler_ran_ = true;
          });
        });
    RunLoopUntil([&codec_impl] { return !!codec_impl; });
    ZX_DEBUG_ASSERT(codec_impl);
    codec_impl_ = std::move(codec_impl);
  }

  void StartSyncChain() {
    if (codec_client_ptr_) {
      codec_client_ptr_->Sync([this] {
        ++sync_completion_count_;
        StartSyncChain();
      });
    }
  }

  void PostToSeparateThread(fit::closure to_run) {
    zx_status_t status = async::PostTask(loop_separate_thread_.dispatcher(), std::move(to_run));
    ZX_DEBUG_ASSERT(status == ZX_OK);
  }

  async::Loop loop_separate_thread_;
  CodecAdmissionControl admission_control_;

  // The server end isn't connected to anything, for now.
  fidl::ClientEnd<fuchsia_sysmem2::Allocator> sysmem_client_;
  fidl::ServerEnd<fuchsia_sysmem2::Allocator> sysmem_request_;

  // Tests that don't intend to process any received messages use
  // InterfaceHandle while those that do intend to process received messages
  // use InterfacePtr.
  fidl::InterfaceHandle<fuchsia::media::StreamProcessor> codec_client_handle_;
  fuchsia::media::StreamProcessorPtr codec_client_ptr_;

  fidl::InterfaceRequest<fuchsia::media::StreamProcessor> codec_request_;

  std::unique_ptr<CodecImpl> codec_impl_;
  FakeCodecAdapter* fake_codec_adapter_;
  bool error_handler_ran_ = false;
  uint64_t sync_completion_count_ = 0;
};

TEST_F(CodecImplLifetime, CreateDelete) {
  Create(false);
  codec_impl_ = nullptr;
  RunLoopUntilIdle();
  EXPECT_FALSE(error_handler_ran_);
}

TEST_F(CodecImplLifetime, CreateBindDelete) {
  // This exercises BindAsync vs. ~CodecImpl, so run several times.
  for (uint32_t i = 0; i < 200; ++i) {
    Create();
    codec_impl_ = nullptr;
    RunLoopUntilIdle();
    EXPECT_FALSE(error_handler_ran_);
  }
}

TEST_F(CodecImplLifetime, CreateBindChannelClose) {
  Create();
  // Close the client end of the StreamProcessor channel.
  codec_client_handle_.TakeChannel().reset();
  RunLoopUntil([this] { return error_handler_ran_; });
  EXPECT_TRUE(error_handler_ran_);
}

TEST_F(CodecImplLifetime, CreateBindChannelCloseDeleteAsync) {
  Create(true, true);
  // Close the client end of the StreamProcessor channel.
  codec_client_handle_.TakeChannel().reset();
  RunLoopUntil([this] { return error_handler_ran_; });
  // This doesn't imply that !codec_impl_ yet, because deletion
  // is happening async.
  EXPECT_TRUE(error_handler_ran_);
  RunLoopUntil([this] { return !codec_impl_; });
  EXPECT_TRUE(!codec_impl_);
}

TEST_F(CodecImplLifetime, CreateBindChannelCloseDeleteAsyncWithOngoingSyncs) {
  // More than one thread is involved, so do this several times in case it helps catch something
  // bad that doesn't always happen.
  constexpr uint64_t kIterCount = 20;
  for (uint64_t iter = 0; iter < kIterCount; ++iter) {
    Create(true, true);

    codec_client_ptr_.Bind(codec_client_handle_.TakeChannel());
    constexpr uint32_t kInFlightSyncTarget = 5;
    for (uint32_t i = 0; i < kInFlightSyncTarget; ++i) {
      // Each started chain kicks another Sync() on each completion, any time
      // RunLoopUntil() is running.
      StartSyncChain();
    }

    // Make sure the sync chains will re-trigger new syncs while RunLoopUntil() is
    // running.
    RunLoopUntil([this] { return sync_completion_count_ >= kInFlightSyncTarget * 2; });

    // Trigger an error as if the FakeCodecAdapter had triggered it, with a slight
    // delay before the trigger so we get some coverage of syncs happening
    // continuously while the failure handling happens.
    PostToSeparateThread([this] {
      zx::nanosleep(zx::deadline_after(zx::msec(20)));
      static_cast<CodecAdapterEvents*>(codec_impl_.get())
          ->onCoreCodecFailCodec(
              "CreateBindChannelCloseDeleteAsyncWithOngoingSyncs triggering failure");
    });

    RunLoopUntil([this] { return !codec_impl_; });
    EXPECT_TRUE(!codec_impl_);
  }
}

TEST_F(CodecImplLifetime, CreateBindDeleteEncoder) {
  // This exercises BindAsync vs. ~CodecImpl, so run several times.
  for (uint32_t i = 0; i < 200; ++i) {
    Create(true, false, CreateEncoderParams());
    codec_impl_ = nullptr;
    RunLoopUntilIdle();
    EXPECT_FALSE(error_handler_ran_);
  }
}

TEST_F(CodecImplLifetime, CreateBindDeleteDecryptor) {
  // This exercises BindAsync vs. ~CodecImpl, so run several times.
  for (uint32_t i = 0; i < 200; ++i) {
    Create(true, false, CreateDecryptorParams());
    codec_impl_ = nullptr;
    RunLoopUntilIdle();
    EXPECT_FALSE(error_handler_ran_);
  }
}

TEST(CodecImplFormatDetailsBridge, VideoV1ToV2) {
  fuchsia::media::FormatDetails format_details;
  format_details.set_format_details_version_ordinal(1);
  format_details.set_mime_type("video/raw");

  fuchsia::media::VideoUncompressedFormat uncompressed;
  uncompressed.image_format.pixel_format.type = fuchsia::sysmem::PixelFormatType::NV12;
  uncompressed.image_format.coded_width = 1920;
  uncompressed.image_format.coded_height = 1088;
  uncompressed.image_format.bytes_per_row = 1920;
  uncompressed.image_format.display_width = 1920;
  uncompressed.image_format.display_height = 1080;
  uncompressed.image_format.layers = 1;
  uncompressed.image_format.color_space.type = fuchsia::sysmem::ColorSpaceType::REC709;
  uncompressed.image_format.has_pixel_aspect_ratio = true;
  uncompressed.image_format.pixel_aspect_ratio_width = 4;
  uncompressed.image_format.pixel_aspect_ratio_height = 3;
  format_details.mutable_domain()->video().set_uncompressed(std::move(uncompressed));

  CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

  ASSERT_TRUE(format_details.has_image_format());
  const auto& v2 = format_details.image_format();
  ASSERT_TRUE(v2.has_pixel_format());
  EXPECT_EQ(v2.pixel_format(), fuchsia::images2::PixelFormat::NV12);
  ASSERT_TRUE(v2.has_size());
  EXPECT_EQ(v2.size().width, 1920u);
  EXPECT_EQ(v2.size().height, 1088u);
  ASSERT_TRUE(v2.has_bytes_per_row());
  EXPECT_EQ(v2.bytes_per_row(), 1920u);
  ASSERT_TRUE(v2.has_display_rect());
  EXPECT_EQ(v2.display_rect().x, 0u);
  EXPECT_EQ(v2.display_rect().y, 0u);
  EXPECT_EQ(v2.display_rect().width, 1920u);
  EXPECT_EQ(v2.display_rect().height, 1080u);
  ASSERT_TRUE(v2.has_color_space());
  EXPECT_EQ(v2.color_space(), fuchsia::images2::ColorSpace::REC709);
  ASSERT_TRUE(v2.has_pixel_aspect_ratio());
  EXPECT_EQ(v2.pixel_aspect_ratio().width, 4u);
  EXPECT_EQ(v2.pixel_aspect_ratio().height, 3u);

  // Bridging V1 -> V2 also normalizes `domain.video.uncompressed` and is strictly idempotent.
  EXPECT_EQ(format_details.domain().video().uncompressed().fourcc, make_fourcc('N', 'V', '1', '2'));
  EXPECT_TRUE(format_details.domain().video().uncompressed().planar);
  EXPECT_EQ(format_details.domain().video().uncompressed().primary_width_pixels, 1920u);
  EXPECT_EQ(format_details.domain().video().uncompressed().primary_height_pixels, 1088u);
  fuchsia::media::FormatDetails bridged_once = fidl::Clone(format_details);
  CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
  EXPECT_TRUE(fidl::Equals(bridged_once, format_details));

  // When V1 omits both display_width/height and primary_display_width/height_pixels,
  // `display_rect` must be unset rather than `{0, 0, 0, 0}`.
  {
    fuchsia::media::FormatDetails no_display_details;
    no_display_details.set_format_details_version_ordinal(2);
    auto& no_disp_v1 = no_display_details.mutable_domain()->video().uncompressed();
    no_disp_v1.image_format.pixel_format.type = fuchsia::sysmem::PixelFormatType::NV12;
    no_disp_v1.image_format.coded_width = 1280;
    no_disp_v1.image_format.coded_height = 720;
    no_disp_v1.image_format.bytes_per_row = 1280;
    no_disp_v1.image_format.color_space.type = fuchsia::sysmem::ColorSpaceType::REC709;

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&no_display_details);
    ASSERT_TRUE(no_display_details.has_image_format());
    EXPECT_FALSE(no_display_details.image_format().has_display_rect());
  }

  // Bridging when `pixel_format` is omitted (`INVALID`) in V1 -> V2 and V2 -> V1.
  {
    fuchsia::media::FormatDetails v1_no_pf;
    v1_no_pf.set_format_details_version_ordinal(3);
    auto& uncompressed_no_pf = v1_no_pf.mutable_domain()->video().uncompressed();
    uncompressed_no_pf.image_format.coded_width = 640;
    uncompressed_no_pf.image_format.coded_height = 480;
    uncompressed_no_pf.image_format.display_width = 640;
    uncompressed_no_pf.image_format.display_height = 480;
    uncompressed_no_pf.image_format.color_space.type = fuchsia::sysmem::ColorSpaceType::REC709;

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&v1_no_pf);
    ASSERT_TRUE(v1_no_pf.has_image_format());
    EXPECT_FALSE(v1_no_pf.image_format().has_pixel_format());
    ASSERT_TRUE(v1_no_pf.image_format().has_size());
    EXPECT_EQ(v1_no_pf.image_format().size().width, 640u);
    EXPECT_EQ(v1_no_pf.image_format().size().height, 480u);
    ASSERT_TRUE(v1_no_pf.image_format().has_display_rect());
    EXPECT_EQ(v1_no_pf.image_format().display_rect().width, 640u);
    EXPECT_EQ(v1_no_pf.image_format().display_rect().height, 480u);

    fuchsia::media::FormatDetails v2_no_pf;
    v2_no_pf.set_format_details_version_ordinal(4);
    v2_no_pf.mutable_image_format()->set_size({.width = 640, .height = 480});
    v2_no_pf.mutable_image_format()->set_display_rect(
        {.x = 0, .y = 0, .width = 640, .height = 480});

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&v2_no_pf);
    ASSERT_TRUE(v2_no_pf.has_domain());
    ASSERT_TRUE(v2_no_pf.domain().is_video());
    ASSERT_TRUE(v2_no_pf.domain().video().is_uncompressed());
    EXPECT_EQ(v2_no_pf.domain().video().uncompressed().image_format.pixel_format.type,
              fuchsia::sysmem::PixelFormatType::INVALID);
    EXPECT_EQ(v2_no_pf.domain().video().uncompressed().image_format.coded_width, 640u);
    EXPECT_EQ(v2_no_pf.domain().video().uncompressed().image_format.coded_height, 480u);
  }
}

TEST(CodecImplFormatDetailsBridge, VideoV2ToV1Nv12AndYv12) {
  {
    fuchsia::media::FormatDetails format_details;
    format_details.set_format_details_version_ordinal(1);
    format_details.set_mime_type("video/raw");

    auto* v2 = format_details.mutable_image_format();
    v2->set_pixel_format(fuchsia::images2::PixelFormat::NV12);
    v2->set_size({.width = 1280, .height = 736});
    v2->set_bytes_per_row(1280);
    v2->set_display_rect({.x = 0, .y = 0, .width = 1280, .height = 720});
    v2->set_color_space(fuchsia::images2::ColorSpace::REC709);
    v2->set_pixel_aspect_ratio({.width = 16, .height = 9});

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.has_domain());
    ASSERT_TRUE(format_details.domain().is_video());
    ASSERT_TRUE(format_details.domain().video().is_uncompressed());
    const auto& v1 = format_details.domain().video().uncompressed();
    EXPECT_EQ(v1.fourcc, make_fourcc('N', 'V', '1', '2'));
    EXPECT_TRUE(v1.planar);
    EXPECT_FALSE(v1.swizzled);
    EXPECT_EQ(v1.primary_width_pixels, 1280u);
    EXPECT_EQ(v1.primary_height_pixels, 736u);
    EXPECT_EQ(v1.secondary_width_pixels, 640u);
    EXPECT_EQ(v1.secondary_height_pixels, 368u);
    EXPECT_EQ(v1.primary_line_stride_bytes, 1280u);
    EXPECT_EQ(v1.secondary_line_stride_bytes, 1280u);
    EXPECT_EQ(v1.primary_start_offset, 0u);
    EXPECT_EQ(v1.secondary_start_offset, 1280u * 736u);
    EXPECT_EQ(v1.tertiary_start_offset, 1280u * 736u + 1u);
    EXPECT_EQ(v1.primary_pixel_stride, 1u);
    EXPECT_EQ(v1.secondary_pixel_stride, 2u);
    EXPECT_EQ(v1.primary_display_width_pixels, 1280u);
    EXPECT_EQ(v1.primary_display_height_pixels, 720u);
    EXPECT_TRUE(v1.has_pixel_aspect_ratio);
    EXPECT_EQ(v1.pixel_aspect_ratio_width, 16u);
    EXPECT_EQ(v1.pixel_aspect_ratio_height, 9u);
    EXPECT_EQ(v1.image_format.pixel_format.type, fuchsia::sysmem::PixelFormatType::NV12);
    EXPECT_EQ(v1.image_format.coded_width, 1280u);
    EXPECT_EQ(v1.image_format.coded_height, 736u);
    EXPECT_EQ(v1.image_format.bytes_per_row, 1280u);
    EXPECT_EQ(v1.image_format.display_width, 1280u);
    EXPECT_EQ(v1.image_format.display_height, 720u);
  }

  {
    fuchsia::media::FormatDetails format_details;
    format_details.set_format_details_version_ordinal(2);
    format_details.set_mime_type("video/raw");

    auto* v2 = format_details.mutable_image_format();
    v2->set_pixel_format(fuchsia::images2::PixelFormat::YV12);
    v2->set_size({.width = 640, .height = 368});
    v2->set_bytes_per_row(640);
    v2->set_display_rect({.x = 0, .y = 0, .width = 640, .height = 360});
    v2->set_color_space(fuchsia::images2::ColorSpace::REC709);

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.has_domain());
    const auto& v1 = format_details.domain().video().uncompressed();
    EXPECT_EQ(v1.fourcc, make_fourcc('Y', 'V', '1', '2'));
    EXPECT_TRUE(v1.planar);
    EXPECT_FALSE(v1.swizzled);
    EXPECT_EQ(v1.primary_width_pixels, 640u);
    EXPECT_EQ(v1.primary_height_pixels, 368u);
    EXPECT_EQ(v1.secondary_width_pixels, 320u);
    EXPECT_EQ(v1.secondary_height_pixels, 184u);
    EXPECT_EQ(v1.primary_line_stride_bytes, 640u);
    EXPECT_EQ(v1.secondary_line_stride_bytes, 320u);
    EXPECT_EQ(v1.primary_start_offset, 0u);
    EXPECT_EQ(v1.secondary_start_offset, 640u * 368u);
    EXPECT_EQ(v1.tertiary_start_offset, 640u * 368u + 320u * 184u);
    EXPECT_EQ(v1.primary_pixel_stride, 1u);
    EXPECT_EQ(v1.secondary_pixel_stride, 1u);
    EXPECT_EQ(v1.primary_display_width_pixels, 640u);
    EXPECT_EQ(v1.primary_display_height_pixels, 360u);
    EXPECT_FALSE(v1.has_pixel_aspect_ratio);

    // Mutating `image_format` on an already-bridged `FormatDetails` updates `domain`.
    format_details.mutable_image_format()->set_size({.width = 1920, .height = 1088});
    format_details.mutable_image_format()->set_bytes_per_row(1920);
    format_details.mutable_image_format()->set_display_rect(
        {.x = 0, .y = 0, .width = 1920, .height = 1080});
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
    EXPECT_EQ(format_details.domain().video().uncompressed().primary_width_pixels, 1920u);
    EXPECT_EQ(format_details.domain().video().uncompressed().primary_display_height_pixels, 1080u);
  }

  {
    // Intel Y-tiled NV12 bridges `swizzled = true` and tile-aligned `secondary_start_offset`.
    fuchsia::media::FormatDetails format_details;
    format_details.set_format_details_version_ordinal(5);
    format_details.set_mime_type("video/raw");

    auto* v2 = format_details.mutable_image_format();
    v2->set_pixel_format(fuchsia::images2::PixelFormat::NV12);
    v2->set_pixel_format_modifier(fuchsia::images2::PixelFormatModifier::INTEL_I915_Y_TILED);
    v2->set_size({.width = 1920, .height = 1080});
    v2->set_bytes_per_row(1920);
    v2->set_display_rect({.x = 0, .y = 0, .width = 1920, .height = 1080});
    v2->set_color_space(fuchsia::images2::ColorSpace::REC709);

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.has_domain());
    ASSERT_TRUE(format_details.domain().is_video());
    ASSERT_TRUE(format_details.domain().video().is_uncompressed());
    const auto& v1 = format_details.domain().video().uncompressed();
    EXPECT_EQ(v1.fourcc, make_fourcc('N', 'V', '1', '2'));
    EXPECT_TRUE(v1.planar);
    EXPECT_TRUE(v1.swizzled);
    EXPECT_EQ(v1.primary_line_stride_bytes, 1920u);
    EXPECT_EQ(v1.secondary_line_stride_bytes, 1920u);
    EXPECT_EQ(v1.secondary_start_offset, 1920u * 1088u);
    EXPECT_EQ(v1.tertiary_start_offset, (1920u * 1088u) + 1u);
    EXPECT_EQ(v1.image_format.pixel_format.has_format_modifier, true);
    EXPECT_EQ(v1.image_format.pixel_format.format_modifier.value,
              fuchsia::sysmem::FORMAT_MODIFIER_INTEL_I915_Y_TILED);
  }

  {
    // R8G8B8A8 bridges with `FakeCodecAdapter`'s constants (256x144, stride 1024).
    fuchsia::media::FormatDetails format_details;
    format_details.set_format_details_version_ordinal(6);
    format_details.set_mime_type("video/raw");

    auto* v2 = format_details.mutable_image_format();
    v2->set_pixel_format(fuchsia::images2::PixelFormat::R8G8B8A8);
    v2->set_color_space(fuchsia::images2::ColorSpace::SRGB);
    v2->set_size({.width = 256, .height = 144});
    v2->set_bytes_per_row(1024);
    v2->set_display_rect({.x = 0, .y = 0, .width = 256, .height = 144});

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.has_domain());
    ASSERT_TRUE(format_details.domain().is_video());
    ASSERT_TRUE(format_details.domain().video().is_uncompressed());
    const auto& v1 = format_details.domain().video().uncompressed();
    EXPECT_EQ(v1.fourcc, make_fourcc('R', 'G', 'B', 'A'));
    EXPECT_FALSE(v1.planar);
    EXPECT_FALSE(v1.swizzled);
    EXPECT_EQ(v1.primary_width_pixels, 256u);
    EXPECT_EQ(v1.primary_height_pixels, 144u);
    EXPECT_EQ(v1.primary_pixel_stride, 4u);
    EXPECT_EQ(v1.primary_line_stride_bytes, 1024u);
    EXPECT_EQ(v1.primary_display_width_pixels, 256u);
    EXPECT_EQ(v1.primary_display_height_pixels, 144u);
  }

  {
    // Overflowing dimensions must not panic or wrap around.
    fuchsia::media::FormatDetails overflow_details;
    overflow_details.set_format_details_version_ordinal(3);
    auto* v2 = overflow_details.mutable_image_format();
    v2->set_pixel_format(fuchsia::images2::PixelFormat::NV12);
    v2->set_pixel_format_modifier(fuchsia::images2::PixelFormatModifier::INTEL_I915_Y_TILED);
    v2->set_size({.width = 65536, .height = 0xFFFFFFF0u});
    v2->set_bytes_per_row(65536);

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&overflow_details);
    EXPECT_FALSE(overflow_details.has_domain());
  }

  {
    // Compressed video `mime_type` synthesizes `domain.video.compressed`.
    fuchsia::media::FormatDetails compressed_video;
    compressed_video.set_format_details_version_ordinal(4);
    compressed_video.set_mime_type("video/h264");

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&compressed_video);
    ASSERT_TRUE(compressed_video.has_domain());
    ASSERT_TRUE(compressed_video.domain().is_video());
    EXPECT_TRUE(compressed_video.domain().video().is_compressed());
  }

  {
    // Linear NV12 and I420 `ImageFormat` omitting optional `bytes_per_row` (and omitting
    // `display_rect` while setting `valid_size`) bridges to unpadded V1 strides and non-overlapping
    // UV plane offsets.
    fuchsia::media::FormatDetails nv12_unpadded;
    nv12_unpadded.set_format_details_version_ordinal(5);
    nv12_unpadded.set_mime_type("video/raw");
    auto* nv12_v2 = nv12_unpadded.mutable_image_format();
    nv12_v2->set_pixel_format(fuchsia::images2::PixelFormat::NV12);
    nv12_v2->set_size({.width = 640, .height = 368});
    nv12_v2->set_valid_size({.width = 640, .height = 360});

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&nv12_unpadded);
    ASSERT_TRUE(nv12_unpadded.has_domain());
    const auto& nv12_v1 = nv12_unpadded.domain().video().uncompressed();
    EXPECT_FALSE(nv12_unpadded.image_format().has_bytes_per_row());
    EXPECT_EQ(nv12_v1.image_format.bytes_per_row, 640u);
    EXPECT_EQ(nv12_v1.primary_line_stride_bytes, 640u);
    EXPECT_EQ(nv12_v1.secondary_line_stride_bytes, 640u);
    EXPECT_EQ(nv12_v1.secondary_start_offset, 640u * 368u);
    EXPECT_EQ(nv12_v1.tertiary_start_offset, 640u * 368u + 1u);
    EXPECT_EQ(nv12_v1.primary_display_width_pixels, 640u);
    EXPECT_EQ(nv12_v1.primary_display_height_pixels, 360u);

    fuchsia::media::FormatDetails nv12_bridged_once = fidl::Clone(nv12_unpadded);
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&nv12_unpadded);
    EXPECT_TRUE(fidl::Equals(nv12_bridged_once, nv12_unpadded));

    fuchsia::media::FormatDetails i420_unpadded;
    i420_unpadded.set_format_details_version_ordinal(6);
    i420_unpadded.set_mime_type("video/raw");
    auto* i420_v2 = i420_unpadded.mutable_image_format();
    i420_v2->set_pixel_format(fuchsia::images2::PixelFormat::I420);
    i420_v2->set_pixel_format_modifier(fuchsia::images2::PixelFormatModifier::LINEAR);
    i420_v2->set_size({.width = 640, .height = 368});

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&i420_unpadded);
    ASSERT_TRUE(i420_unpadded.has_domain());
    const auto& i420_v1 = i420_unpadded.domain().video().uncompressed();
    EXPECT_EQ(i420_v1.primary_line_stride_bytes, 640u);
    EXPECT_EQ(i420_v1.secondary_line_stride_bytes, 320u);
    EXPECT_EQ(i420_v1.secondary_start_offset, 640u * 368u);
    EXPECT_EQ(i420_v1.tertiary_start_offset, 640u * 368u + 320u * 184u);
  }
}

TEST(CodecImplFormatDetailsBridge, AudioV1ToV2AndV2ToV1) {
  {
    fuchsia::media::FormatDetails format_details;
    format_details.set_format_details_version_ordinal(1);
    format_details.set_mime_type("audio/raw");

    fuchsia::media::PcmFormat pcm_v1;
    pcm_v1.pcm_mode = fuchsia::media::AudioPcmMode::LINEAR;
    pcm_v1.bits_per_sample = 16;
    pcm_v1.frames_per_second = 48000;
    pcm_v1.channel_map = {
        fuchsia::media::AudioChannelId::LF, fuchsia::media::AudioChannelId::RF,
        fuchsia::media::AudioChannelId::CF, fuchsia::media::AudioChannelId::LFE,
        fuchsia::media::AudioChannelId::LR, fuchsia::media::AudioChannelId::RR,
        fuchsia::media::AudioChannelId::LS, fuchsia::media::AudioChannelId::RS,
        fuchsia::media::AudioChannelId::CS,
    };
    format_details.mutable_domain()->audio().uncompressed().set_pcm(std::move(pcm_v1));

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.has_pcm_format());
    const auto& pcm_v2 = format_details.pcm_format();
    ASSERT_TRUE(pcm_v2.has_pcm_mode());
    EXPECT_EQ(pcm_v2.pcm_mode(), fuchsia::media::AudioPcmMode2::LINEAR);
    ASSERT_TRUE(pcm_v2.has_bits_per_sample());
    EXPECT_EQ(pcm_v2.bits_per_sample(), 16u);
    ASSERT_TRUE(pcm_v2.has_frames_per_second());
    EXPECT_EQ(pcm_v2.frames_per_second(), 48000u);
    ASSERT_TRUE(pcm_v2.has_channel_map());
    std::vector<fuchsia::media::AudioChannelId2> expected_v2 = {
        fuchsia::media::AudioChannelId2::FL, fuchsia::media::AudioChannelId2::FR,
        fuchsia::media::AudioChannelId2::FC, fuchsia::media::AudioChannelId2::LFE,
        fuchsia::media::AudioChannelId2::BL, fuchsia::media::AudioChannelId2::BR,
        fuchsia::media::AudioChannelId2::SL, fuchsia::media::AudioChannelId2::SR,
        fuchsia::media::AudioChannelId2::BC,
    };
    EXPECT_EQ(pcm_v2.channel_map(), expected_v2);
    ExpectFormatDetailsFidlValid(format_details);
  }

  {
    fuchsia::media::FormatDetails format_details;
    format_details.set_format_details_version_ordinal(2);
    format_details.set_mime_type("audio/raw");

    auto* pcm_v2 = format_details.mutable_pcm_format();
    pcm_v2->set_pcm_mode(fuchsia::media::AudioPcmMode2::LINEAR);
    pcm_v2->set_bits_per_sample(24);
    pcm_v2->set_frames_per_second(96000);
    pcm_v2->set_channel_map({
        fuchsia::media::AudioChannelId2::FL,
        fuchsia::media::AudioChannelId2::FR,
        fuchsia::media::AudioChannelId2::BC,
    });

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
    ExpectFormatDetailsFidlValid(format_details);

    ASSERT_TRUE(format_details.has_domain());
    ASSERT_TRUE(format_details.domain().is_audio());
    ASSERT_TRUE(format_details.domain().audio().is_uncompressed());
    ASSERT_TRUE(format_details.domain().audio().uncompressed().is_pcm());
    const auto& pcm_v1 = format_details.domain().audio().uncompressed().pcm();
    EXPECT_EQ(pcm_v1.pcm_mode, fuchsia::media::AudioPcmMode::LINEAR);
    EXPECT_EQ(pcm_v1.bits_per_sample, 24u);
    EXPECT_EQ(pcm_v1.frames_per_second, 96000u);
    std::vector<fuchsia::media::AudioChannelId> expected_v1 = {
        fuchsia::media::AudioChannelId::LF,
        fuchsia::media::AudioChannelId::RF,
        fuchsia::media::AudioChannelId::CS,
    };
    EXPECT_EQ(pcm_v1.channel_map, expected_v1);

    // Mutating `pcm_format` on an already-bridged `FormatDetails` updates `domain`.
    format_details.mutable_pcm_format()->set_frames_per_second(44100);
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
    ExpectFormatDetailsFidlValid(format_details);
    EXPECT_EQ(format_details.domain().audio().uncompressed().pcm().frames_per_second, 44100u);
  }

  {
    // Spatial channels (e.g. TFL) that cannot be represented in legacy AudioChannelId
    // should not synthesize a legacy PcmFormat.
    fuchsia::media::FormatDetails format_details;
    format_details.set_format_details_version_ordinal(3);
    format_details.set_mime_type("audio/raw");

    auto* pcm_v2 = format_details.mutable_pcm_format();
    pcm_v2->set_pcm_mode(fuchsia::media::AudioPcmMode2::LINEAR);
    pcm_v2->set_bits_per_sample(16);
    pcm_v2->set_frames_per_second(48000);
    pcm_v2->set_channel_map({
        fuchsia::media::AudioChannelId2::FL,
        fuchsia::media::AudioChannelId2::FR,
        fuchsia::media::AudioChannelId2::TFL,
    });

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
    EXPECT_FALSE(format_details.has_domain());
    ExpectFormatDetailsFidlValid(format_details);
  }

  {
    // Extended V2 channel IDs `[MIN_EXTENDED_AUDIO_CHANNEL_ID2, MAX_AUDIO_CHANNEL_ID2]` are
    // V2-only and must not synthesize a legacy `PcmFormat` (which uses a `strict enum` that rejects
    // unlisted extended values on FIDL wire validation).
    for (uint32_t raw_id : {
             fuchsia::media::MIN_EXTENDED_AUDIO_CHANNEL_ID2,
             fuchsia::media::MIN_EXTENDED_AUDIO_CHANNEL_ID2 + 1,
             fuchsia::media::MAX_AUDIO_CHANNEL_ID2 - 1,
             fuchsia::media::MAX_AUDIO_CHANNEL_ID2,
         }) {
      fuchsia::media::FormatDetails format_details;
      format_details.set_format_details_version_ordinal(3);
      format_details.set_mime_type("audio/raw");

      auto* pcm_v2 = format_details.mutable_pcm_format();
      pcm_v2->set_pcm_mode(fuchsia::media::AudioPcmMode2::LINEAR);
      pcm_v2->set_bits_per_sample(16);
      pcm_v2->set_frames_per_second(48000);
      pcm_v2->set_channel_map({
          fuchsia::media::AudioChannelId2::FL,
          static_cast<fuchsia::media::AudioChannelId2>(raw_id),
      });

      CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
      EXPECT_FALSE(format_details.has_domain()) << "raw_id=0x" << std::hex << raw_id;
      EXPECT_TRUE(format_details.has_pcm_format()) << "raw_id=0x" << std::hex << raw_id;
      ExpectFormatDetailsFidlValid(format_details);
    }
  }

  {
    // Non-role legacy `AudioChannelId` range markers (`END_DEFINED`, `EXTENDED_CHANNEL_ID_BASE`,
    // `MAX`) and unlisted values do not populate `pcm_format`.
    for (fuchsia::media::AudioChannelId sentinel : {
             fuchsia::media::AudioChannelId::END_DEFINED,
             fuchsia::media::AudioChannelId::EXTENDED_CHANNEL_ID_BASE,
             fuchsia::media::AudioChannelId::MAX,
         }) {
      fuchsia::media::FormatDetails format_details;
      format_details.set_format_details_version_ordinal(3);
      format_details.set_mime_type("audio/raw");
      fuchsia::media::PcmFormat pcm_v1;
      pcm_v1.pcm_mode = fuchsia::media::AudioPcmMode::LINEAR;
      pcm_v1.bits_per_sample = 16;
      pcm_v1.frames_per_second = 48000;
      pcm_v1.channel_map = {sentinel};
      format_details.mutable_domain()->audio().uncompressed().set_pcm(std::move(pcm_v1));

      CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
      EXPECT_FALSE(format_details.has_pcm_format());
      ExpectFormatDetailsFidlValid(format_details);
    }

    fuchsia::media::FormatDetails v1_unlisted;
    v1_unlisted.set_format_details_version_ordinal(3);
    v1_unlisted.set_mime_type("audio/raw");
    fuchsia::media::PcmFormat pcm_v1;
    pcm_v1.pcm_mode = fuchsia::media::AudioPcmMode::LINEAR;
    pcm_v1.bits_per_sample = 16;
    pcm_v1.frames_per_second = 48000;
    pcm_v1.channel_map = {
        static_cast<fuchsia::media::AudioChannelId>(fuchsia::media::MIN_EXTENDED_AUDIO_CHANNEL_ID2 +
                                                    1),
    };
    v1_unlisted.mutable_domain()->audio().uncompressed().set_pcm(std::move(pcm_v1));
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&v1_unlisted);
    EXPECT_FALSE(v1_unlisted.has_pcm_format());
  }

  {
    // Compressed audio `mime_type` synthesizes `domain.audio.compressed`.
    fuchsia::media::FormatDetails compressed_audio;
    compressed_audio.set_format_details_version_ordinal(4);
    compressed_audio.set_mime_type("audio/aac");

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&compressed_audio);
    ASSERT_TRUE(compressed_audio.has_domain());
    ASSERT_TRUE(compressed_audio.domain().is_audio());
    EXPECT_TRUE(compressed_audio.domain().audio().is_compressed());
    ExpectFormatDetailsFidlValid(compressed_audio);
  }

  {
    // Legacy V1 24-bit `PcmFormat` paired with `encoder_settings.lc3` bridges to explicit
    // `bytes_per_sample == 4` and stays idempotent on repeated calls.
    fuchsia::media::FormatDetails lc3_v1;
    lc3_v1.set_format_details_version_ordinal(5);
    lc3_v1.set_mime_type("audio/raw");
    lc3_v1.mutable_encoder_settings()->set_lc3({});
    fuchsia::media::PcmFormat pcm_v1;
    pcm_v1.pcm_mode = fuchsia::media::AudioPcmMode::LINEAR;
    pcm_v1.bits_per_sample = 24;
    pcm_v1.frames_per_second = 48000;
    pcm_v1.channel_map = {fuchsia::media::AudioChannelId::LF};
    lc3_v1.mutable_domain()->audio().uncompressed().set_pcm(std::move(pcm_v1));

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&lc3_v1);
    ASSERT_TRUE(lc3_v1.has_pcm_format());
    ASSERT_TRUE(lc3_v1.pcm_format().has_bytes_per_sample());
    EXPECT_EQ(lc3_v1.pcm_format().bytes_per_sample(), 4u);
    EXPECT_EQ(lc3_v1.pcm_format().channel_map()[0], fuchsia::media::AudioChannelId2::FL);
    ASSERT_TRUE(lc3_v1.has_domain());
    ExpectFormatDetailsFidlValid(lc3_v1);

    fuchsia::media::FormatDetails bridged_once = fidl::Clone(lc3_v1);
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&lc3_v1);
    EXPECT_TRUE(fidl::Equals(bridged_once, lc3_v1));

    // Packed 24-bit LC3 (`bytes_per_sample == 3` or omitted) cannot be represented in legacy V1
    // `PcmFormat` (where 24-bit LC3 implied 4-byte containers), so `domain` must be cleared.
    fuchsia::media::FormatDetails lc3_packed_explicit = fidl::Clone(lc3_v1);
    lc3_packed_explicit.mutable_pcm_format()->set_bytes_per_sample(3);
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&lc3_packed_explicit);
    EXPECT_FALSE(lc3_packed_explicit.has_domain());
    EXPECT_TRUE(lc3_packed_explicit.has_pcm_format());
    ExpectFormatDetailsFidlValid(lc3_packed_explicit);

    fuchsia::media::FormatDetails lc3_packed_implicit = fidl::Clone(lc3_v1);
    lc3_packed_implicit.mutable_pcm_format()->clear_bytes_per_sample();
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&lc3_packed_implicit);
    EXPECT_FALSE(lc3_packed_implicit.has_domain());
    EXPECT_TRUE(lc3_packed_implicit.has_pcm_format());
    ExpectFormatDetailsFidlValid(lc3_packed_implicit);
  }

  {
    // Oversized in-process V1 `PcmFormat.channel_map` (> 16 channels) must not populate
    // `pcm_format`.
    fuchsia::media::FormatDetails oversized_v1;
    oversized_v1.set_format_details_version_ordinal(6);
    oversized_v1.set_mime_type("audio/raw");
    fuchsia::media::PcmFormat pcm_v1;
    pcm_v1.pcm_mode = fuchsia::media::AudioPcmMode::LINEAR;
    pcm_v1.bits_per_sample = 16;
    pcm_v1.frames_per_second = 48000;
    pcm_v1.channel_map.assign(17, fuchsia::media::AudioChannelId::SKIP);
    oversized_v1.mutable_domain()->audio().uncompressed().set_pcm(std::move(pcm_v1));

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&oversized_v1);
    EXPECT_FALSE(oversized_v1.has_pcm_format());
  }
}

TEST(CodecImplFormatDetailsBridge, CryptoV1ToV2AndV2ToV1) {
  {
    fuchsia::media::FormatDetails format_details;
    format_details.set_format_details_version_ordinal(1);
    auto* enc_v1 = &format_details.mutable_domain()->crypto().encrypted();
    enc_v1->set_scheme("cenc");
    enc_v1->set_key_id({1, 2, 3, 4});
    enc_v1->set_init_vector({5, 6, 7, 8});

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.has_encrypted_format());
    const auto& enc_v2 = format_details.encrypted_format();
    ASSERT_TRUE(enc_v2.has_scheme());
    EXPECT_EQ(enc_v2.scheme(), "cenc");
    ASSERT_TRUE(enc_v2.has_key_id());
    EXPECT_EQ(enc_v2.key_id(), (std::vector<uint8_t>{1, 2, 3, 4}));
    ASSERT_TRUE(enc_v2.has_init_vector());
    EXPECT_EQ(enc_v2.init_vector(), (std::vector<uint8_t>{5, 6, 7, 8}));
  }

  {
    fuchsia::media::FormatDetails format_details;
    format_details.set_format_details_version_ordinal(2);
    auto* enc_v2 = format_details.mutable_encrypted_format();
    enc_v2->set_scheme("cbcs");
    enc_v2->set_key_id({9, 10, 11, 12});

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.has_domain());
    ASSERT_TRUE(format_details.domain().is_crypto());
    ASSERT_TRUE(format_details.domain().crypto().is_encrypted());
    const auto& enc_v1 = format_details.domain().crypto().encrypted();
    ASSERT_TRUE(enc_v1.has_scheme());
    EXPECT_EQ(enc_v1.scheme(), "cbcs");
    ASSERT_TRUE(enc_v1.has_key_id());
    EXPECT_EQ(enc_v1.key_id(), (std::vector<uint8_t>{9, 10, 11, 12}));

    // Mutating `encrypted_format` on an already-bridged `FormatDetails` updates `domain`.
    format_details.mutable_encrypted_format()->set_scheme("cenc");
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
    EXPECT_EQ(format_details.domain().crypto().encrypted().scheme(), "cenc");
  }
}

TEST(CodecImplFormatDetailsBridge, VideoV1ToV2OptionalFieldOmission) {
  {
    // Display-only V1 metadata with default/zero coded dimensions, stride, and INVALID color space
    // must leave `color_space`, `bytes_per_row`, `size`, and `valid_size` unset on V2
    // `image_format`.
    fuchsia::media::FormatDetails format_details;
    format_details.set_format_details_version_ordinal(1);
    format_details.set_mime_type("video/raw");

    auto& uncompressed = format_details.mutable_domain()->video().uncompressed();
    uncompressed.image_format.pixel_format.type = fuchsia::sysmem::PixelFormatType::NV12;
    uncompressed.image_format.coded_width = 0;
    uncompressed.image_format.coded_height = 0;
    uncompressed.image_format.bytes_per_row = 0;
    uncompressed.image_format.display_width = 640;
    uncompressed.image_format.display_height = 480;
    uncompressed.image_format.color_space.type = fuchsia::sysmem::ColorSpaceType::INVALID;

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.has_image_format());
    const auto& v2 = format_details.image_format();
    ASSERT_TRUE(v2.has_pixel_format());
    EXPECT_EQ(v2.pixel_format(), fuchsia::images2::PixelFormat::NV12);
    EXPECT_FALSE(v2.has_color_space());
    EXPECT_FALSE(v2.has_bytes_per_row());
    EXPECT_FALSE(v2.has_size());
    EXPECT_FALSE(v2.has_valid_size());
    ASSERT_TRUE(v2.has_display_rect());
    EXPECT_EQ(v2.display_rect().x, 0u);
    EXPECT_EQ(v2.display_rect().y, 0u);
    EXPECT_EQ(v2.display_rect().width, 640u);
    EXPECT_EQ(v2.display_rect().height, 480u);

    fuchsia::media::FormatDetails bridged_once = fidl::Clone(format_details);
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
    EXPECT_TRUE(fidl::Equals(bridged_once, format_details));
    ExpectFormatDetailsFidlValid(format_details);
  }

  {
    // Legacy V1 encoder input omitting `pixel_format`, `color_space`, and `bytes_per_row` while
    // specifying coded dimensions leaves those optional V2 fields unset.
    fuchsia::media::FormatDetails format_details;
    format_details.set_format_details_version_ordinal(2);
    format_details.set_mime_type("video/raw");

    auto& uncompressed = format_details.mutable_domain()->video().uncompressed();
    uncompressed.image_format.coded_width = 1280;
    uncompressed.image_format.coded_height = 720;
    uncompressed.image_format.bytes_per_row = 0;
    uncompressed.image_format.color_space.type = fuchsia::sysmem::ColorSpaceType::INVALID;

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.has_image_format());
    const auto& v2 = format_details.image_format();
    EXPECT_FALSE(v2.has_pixel_format());
    EXPECT_FALSE(v2.has_color_space());
    EXPECT_FALSE(v2.has_bytes_per_row());
    EXPECT_FALSE(v2.has_display_rect());
    ASSERT_TRUE(v2.has_size());
    EXPECT_EQ(v2.size().width, 1280u);
    EXPECT_EQ(v2.size().height, 720u);
    ExpectFormatDetailsFidlValid(format_details);
  }
}

TEST(CodecImplFormatDetailsBridge, ClearsStaleDomainOnV2OnlyMutation) {
  // Video: start from a bridged V2+V1 FormatDetails, then mutate `image_format` to V2-only values.
  {
    fuchsia::media::FormatDetails base_video;
    base_video.set_format_details_version_ordinal(1);
    base_video.set_mime_type("video/raw");
    auto* v2 = base_video.mutable_image_format();
    v2->set_pixel_format(fuchsia::images2::PixelFormat::NV12);
    v2->set_size({.width = 1280, .height = 720});
    v2->set_bytes_per_row(1280);
    v2->set_display_rect({.x = 0, .y = 0, .width = 1280, .height = 720});
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&base_video);
    ASSERT_TRUE(base_video.has_domain());
    ExpectFormatDetailsFidlValid(base_video);

    // Non-zero `display_rect.x` clears stale `domain`.
    fuchsia::media::FormatDetails cropped_x = fidl::Clone(base_video);
    cropped_x.mutable_image_format()->set_display_rect(
        {.x = 4, .y = 0, .width = 1276, .height = 720});
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&cropped_x);
    EXPECT_FALSE(cropped_x.has_domain());
    EXPECT_TRUE(cropped_x.has_image_format());
    ExpectFormatDetailsFidlValid(cropped_x);

    // Non-zero `display_rect.y` clears stale `domain`.
    fuchsia::media::FormatDetails cropped_y = fidl::Clone(base_video);
    cropped_y.mutable_image_format()->set_display_rect(
        {.x = 0, .y = 2, .width = 1280, .height = 718});
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&cropped_y);
    EXPECT_FALSE(cropped_y.has_domain());
    ExpectFormatDetailsFidlValid(cropped_y);

    // V2-only `PixelFormat::P010` clears stale `domain`.
    fuchsia::media::FormatDetails p010 = fidl::Clone(base_video);
    p010.mutable_image_format()->set_pixel_format(fuchsia::images2::PixelFormat::P010);
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&p010);
    EXPECT_FALSE(p010.has_domain());
    ExpectFormatDetailsFidlValid(p010);

    // V2-only `ColorSpace` clears stale `domain`.
    fuchsia::media::FormatDetails unknown_cs = fidl::Clone(base_video);
    unknown_cs.mutable_image_format()->set_color_space(
        static_cast<fuchsia::images2::ColorSpace>(100));
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&unknown_cs);
    EXPECT_FALSE(unknown_cs.has_domain());
    ExpectFormatDetailsFidlValid(unknown_cs);
  }

  // Audio: start from a bridged V2+V1 FormatDetails, then mutate `pcm_format` to V2-only values.
  {
    fuchsia::media::FormatDetails base_audio;
    base_audio.set_format_details_version_ordinal(1);
    base_audio.set_mime_type("audio/raw");
    auto* pcm_v2 = base_audio.mutable_pcm_format();
    pcm_v2->set_pcm_mode(fuchsia::media::AudioPcmMode2::LINEAR);
    pcm_v2->set_bits_per_sample(24);
    pcm_v2->set_frames_per_second(48000);
    pcm_v2->set_channel_map({
        fuchsia::media::AudioChannelId2::FL,
        fuchsia::media::AudioChannelId2::FR,
        fuchsia::media::AudioChannelId2::FC,
    });
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&base_audio);
    ASSERT_TRUE(base_audio.has_domain());
    ExpectFormatDetailsFidlValid(base_audio);

    // Packed 24-bit (`bytes_per_sample == 3`) remains representable in V1.
    fuchsia::media::FormatDetails packed_24 = fidl::Clone(base_audio);
    packed_24.mutable_pcm_format()->set_bytes_per_sample(3);
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&packed_24);
    EXPECT_TRUE(packed_24.has_domain());
    ExpectFormatDetailsFidlValid(packed_24);

    // 24-in-32 padded (`bits_per_sample == 24`, `bytes_per_sample == 4`) clears stale `domain`.
    fuchsia::media::FormatDetails padded_24_in_32 = fidl::Clone(base_audio);
    padded_24_in_32.mutable_pcm_format()->set_bytes_per_sample(4);
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&padded_24_in_32);
    EXPECT_FALSE(padded_24_in_32.has_domain());
    EXPECT_TRUE(padded_24_in_32.has_pcm_format());
    ExpectFormatDetailsFidlValid(padded_24_in_32);

    // Wide spatial channels (`FWL` / `FWR`) clear stale `domain`.
    fuchsia::media::FormatDetails wide_channels = fidl::Clone(base_audio);
    wide_channels.mutable_pcm_format()->set_channel_map({
        fuchsia::media::AudioChannelId2::FL,
        fuchsia::media::AudioChannelId2::FR,
        fuchsia::media::AudioChannelId2::FWL,
        fuchsia::media::AudioChannelId2::FWR,
    });
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&wide_channels);
    EXPECT_FALSE(wide_channels.has_domain());
    ExpectFormatDetailsFidlValid(wide_channels);

    // Extended channel IDs `[MIN_EXTENDED_AUDIO_CHANNEL_ID2, MAX_AUDIO_CHANNEL_ID2]` clear stale
    // `domain` and remain FIDL-valid.
    for (uint32_t raw_id : {
             fuchsia::media::MIN_EXTENDED_AUDIO_CHANNEL_ID2,
             fuchsia::media::MIN_EXTENDED_AUDIO_CHANNEL_ID2 + 1,
             fuchsia::media::MAX_AUDIO_CHANNEL_ID2,
         }) {
      fuchsia::media::FormatDetails extended_channel = fidl::Clone(base_audio);
      extended_channel.mutable_pcm_format()->set_channel_map({
          fuchsia::media::AudioChannelId2::FL,
          static_cast<fuchsia::media::AudioChannelId2>(raw_id),
      });
      CodecImpl::EnsureFormatDetailsBridgedForTesting(&extended_channel);
      EXPECT_FALSE(extended_channel.has_domain()) << "raw_id=0x" << std::hex << raw_id;
      EXPECT_TRUE(extended_channel.has_pcm_format()) << "raw_id=0x" << std::hex << raw_id;
      ExpectFormatDetailsFidlValid(extended_channel);
    }

    // > 16 channels clears stale `domain`.
    fuchsia::media::FormatDetails seventeen_channels = fidl::Clone(base_audio);
    seventeen_channels.mutable_pcm_format()->set_channel_map(
        std::vector<fuchsia::media::AudioChannelId2>(17, fuchsia::media::AudioChannelId2::SKIP));
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&seventeen_channels);
    EXPECT_FALSE(seventeen_channels.has_domain());
    ExpectFormatDetailsFidlValid(seventeen_channels);

    // `AudioPcmMode2::FLOAT` clears stale `domain`.
    fuchsia::media::FormatDetails float_pcm = fidl::Clone(base_audio);
    float_pcm.mutable_pcm_format()->set_pcm_mode(fuchsia::media::AudioPcmMode2::FLOAT);
    float_pcm.mutable_pcm_format()->set_bits_per_sample(32);
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&float_pcm);
    EXPECT_FALSE(float_pcm.has_domain());
    ExpectFormatDetailsFidlValid(float_pcm);

    // Missing required field clears stale `domain`.
    fuchsia::media::FormatDetails missing_fps = fidl::Clone(base_audio);
    missing_fps.mutable_pcm_format()->clear_frames_per_second();
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&missing_fps);
    EXPECT_FALSE(missing_fps.has_domain());
    ExpectFormatDetailsFidlValid(missing_fps);

    // `bits_per_sample == 0` clears stale `domain`.
    fuchsia::media::FormatDetails zero_bits = fidl::Clone(base_audio);
    zero_bits.mutable_pcm_format()->set_bits_per_sample(0);
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&zero_bits);
    EXPECT_FALSE(zero_bits.has_domain());
    ExpectFormatDetailsFidlValid(zero_bits);
  }
}

TEST(CodecImplFormatDetailsBridge, InputNormalizationMovesAndClearsDomain) {
  // Crypto: V1 `domain.crypto.encrypted` moves buffers into `encrypted_format` and clears `domain`,
  // matching an equivalent V2 `FormatDetails` via `fidl::Equals`.
  {
    fuchsia::media::FormatDetails v1_crypto;
    v1_crypto.set_format_details_version_ordinal(1);
    auto* enc_v1 = &v1_crypto.mutable_domain()->crypto().encrypted();
    enc_v1->set_scheme("cenc");
    enc_v1->set_key_id({1, 2, 3, 4});
    enc_v1->set_init_vector({5, 6, 7, 8});
    enc_v1->set_subsamples({{.clear_bytes = 16, .encrypted_bytes = 48}});
    const uint8_t* key_id_ptr = enc_v1->key_id().data();
    const uint8_t* iv_ptr = enc_v1->init_vector().data();

    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&v1_crypto);
    EXPECT_FALSE(v1_crypto.has_domain());
    ASSERT_TRUE(v1_crypto.has_encrypted_format());
    EXPECT_EQ(v1_crypto.encrypted_format().key_id().data(), key_id_ptr);
    EXPECT_EQ(v1_crypto.encrypted_format().init_vector().data(), iv_ptr);

    fuchsia::media::FormatDetails v2_crypto;
    v2_crypto.set_format_details_version_ordinal(1);
    auto* enc_v2 = v2_crypto.mutable_encrypted_format();
    enc_v2->set_scheme("cenc");
    enc_v2->set_key_id({1, 2, 3, 4});
    enc_v2->set_init_vector({5, 6, 7, 8});
    enc_v2->set_subsamples({{.clear_bytes = 16, .encrypted_bytes = 48}});

    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&v2_crypto);
    EXPECT_FALSE(v2_crypto.has_domain());
    ASSERT_TRUE(v2_crypto.has_encrypted_format());
    EXPECT_TRUE(fidl::Equals(v1_crypto, v2_crypto));
  }

  // Video and audio: input normalization preserves bidirectional bridging during the multi-repo
  // encoder soft transition so V1 and V2 callers still compare equal via `fidl::Equals` while
  // keeping `domain` populated for encoders in petal repos that have not yet rolled.
  {
    fuchsia::media::FormatDetails v1_video;
    v1_video.set_format_details_version_ordinal(2);
    v1_video.set_mime_type("video/raw");
    auto& uncompressed = v1_video.mutable_domain()->video().uncompressed();
    uncompressed.image_format.pixel_format.type = fuchsia::sysmem::PixelFormatType::NV12;
    uncompressed.image_format.coded_width = 1280;
    uncompressed.image_format.coded_height = 720;
    uncompressed.image_format.bytes_per_row = 1280;
    uncompressed.image_format.display_width = 1280;
    uncompressed.image_format.display_height = 720;
    uncompressed.image_format.color_space.type = fuchsia::sysmem::ColorSpaceType::REC709;

    fuchsia::media::FormatDetails v2_video;
    v2_video.set_format_details_version_ordinal(2);
    v2_video.set_mime_type("video/raw");
    auto* img_v2 = v2_video.mutable_image_format();
    img_v2->set_pixel_format(fuchsia::images2::PixelFormat::NV12);
    img_v2->set_size({.width = 1280, .height = 720});
    img_v2->set_valid_size({.width = 1280, .height = 720});
    img_v2->set_bytes_per_row(1280);
    img_v2->set_display_rect({.x = 0, .y = 0, .width = 1280, .height = 720});
    img_v2->set_color_space(fuchsia::images2::ColorSpace::REC709);

    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&v1_video);
    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&v2_video);
    EXPECT_TRUE(v1_video.has_domain());
    ASSERT_TRUE(v1_video.has_image_format());
    EXPECT_TRUE(v2_video.has_domain());
    EXPECT_TRUE(fidl::Equals(v1_video, v2_video));
  }

  {
    fuchsia::media::FormatDetails v1_audio;
    v1_audio.set_format_details_version_ordinal(3);
    v1_audio.set_mime_type("audio/raw");
    fuchsia::media::PcmFormat pcm_v1;
    pcm_v1.pcm_mode = fuchsia::media::AudioPcmMode::LINEAR;
    pcm_v1.bits_per_sample = 16;
    pcm_v1.frames_per_second = 48000;
    pcm_v1.channel_map = {
        fuchsia::media::AudioChannelId::LF,
        fuchsia::media::AudioChannelId::RF,
    };
    v1_audio.mutable_domain()->audio().uncompressed().set_pcm(std::move(pcm_v1));

    fuchsia::media::FormatDetails v2_audio;
    v2_audio.set_format_details_version_ordinal(3);
    v2_audio.set_mime_type("audio/raw");
    auto* pcm_v2 = v2_audio.mutable_pcm_format();
    pcm_v2->set_pcm_mode(fuchsia::media::AudioPcmMode2::LINEAR);
    pcm_v2->set_bits_per_sample(16);
    pcm_v2->set_frames_per_second(48000);
    pcm_v2->set_channel_map({
        fuchsia::media::AudioChannelId2::FL,
        fuchsia::media::AudioChannelId2::FR,
    });

    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&v1_audio);
    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&v2_audio);
    EXPECT_TRUE(v1_audio.has_domain());
    ASSERT_TRUE(v1_audio.has_pcm_format());
    EXPECT_TRUE(v2_audio.has_domain());
    EXPECT_TRUE(fidl::Equals(v1_audio, v2_audio));
  }

  // Decryptor input with `mime_type` set (and no `encrypted_format` yet, e.g. on CreateDecryptor)
  // must NOT synthesize `domain.video.compressed`.
  {
    fuchsia::media::FormatDetails decryptor_init;
    decryptor_init.set_format_details_version_ordinal(0);
    decryptor_init.set_mime_type("video/h264");

    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&decryptor_init, /*is_decryptor=*/true);
    EXPECT_FALSE(decryptor_init.has_domain());
    EXPECT_FALSE(decryptor_init.has_encrypted_format());
  }

  // Secure decoder input (`is_decryptor == false`) with `mime_type` + `encrypted_format` (or legacy
  // `domain.crypto.encrypted`) synthesizes `domain.video.compressed` while keeping
  // `encrypted_format` populated without cloning into `domain.crypto`.
  {
    fuchsia::media::FormatDetails secure_decoder_v2;
    secure_decoder_v2.set_format_details_version_ordinal(4);
    secure_decoder_v2.set_mime_type("video/h264");
    secure_decoder_v2.mutable_encrypted_format()->set_scheme("cenc").set_key_id({1, 2, 3, 4});

    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&secure_decoder_v2,
                                                            /*is_decryptor=*/false);
    ASSERT_TRUE(secure_decoder_v2.has_encrypted_format());
    ASSERT_TRUE(secure_decoder_v2.has_domain());
    ASSERT_TRUE(secure_decoder_v2.domain().is_video());
    EXPECT_TRUE(secure_decoder_v2.domain().video().is_compressed());

    fuchsia::media::FormatDetails secure_decoder_v1;
    secure_decoder_v1.set_format_details_version_ordinal(4);
    secure_decoder_v1.set_mime_type("video/h264");
    secure_decoder_v1.mutable_domain()->crypto().encrypted().set_scheme("cenc").set_key_id(
        {1, 2, 3, 4});

    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&secure_decoder_v1,
                                                            /*is_decryptor=*/false);
    EXPECT_TRUE(fidl::Equals(secure_decoder_v1, secure_decoder_v2));
  }
}
