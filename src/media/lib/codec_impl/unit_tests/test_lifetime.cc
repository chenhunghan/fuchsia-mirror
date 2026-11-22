// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fake_codec_adapter.h>
#include <fidl/fuchsia.images2/cpp/fidl.h>
#include <fidl/fuchsia.media.drm/cpp/fidl.h>
#include <fidl/fuchsia.media/cpp/fidl.h>
#include <fidl/fuchsia.mediacodec/cpp/fidl.h>
#include <fidl/fuchsia.sysmem/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/media/codec_impl/fourcc.h>
#include <lib/zx/time.h>

#include <gtest/gtest.h>

#include "lib/media/codec_impl/codec_impl.h"
#include "src/lib/testing/loop_fixture/real_loop_fixture.h"

namespace {

auto CreateDecoderParams() {
  fuchsia_mediacodec::CreateDecoderParams params;
  auto& details = params.input_details().emplace();
  details.format_details_version_ordinal() = 0;
  details.mime_type() = "video/vp9";
  return params;
}

auto CreateEncoderParams() {
  fuchsia_mediacodec::CreateEncoderParams params;
  auto& details = params.input_details().emplace();
  details.format_details_version_ordinal() = 0;
  details.mime_type() = "audio/sbc";
  return params;
}

auto CreateDecryptorParams() {
  fuchsia_media_drm::DecryptorParams params;
  params.input_details().emplace().format_details_version_ordinal() = 0;
  return params;
}

void ExpectFormatDetailsFidlValid(const fuchsia_media::FormatDetails& format_details) {
  auto persist_result = fidl::Persist(format_details);
  EXPECT_TRUE(persist_result.is_ok()) << persist_result.error_value().FormatDescription();
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
              CodecImpl::NaturalStreamProcessorParams params = CreateDecoderParams()) {
    // Just hold onto the server end and never connect it to anything, for now.
    auto sysmem_endpoints = fidl::CreateEndpoints<fuchsia_sysmem2::Allocator>();
    ZX_ASSERT(sysmem_endpoints.is_ok());
    sysmem_client_ = std::move(sysmem_endpoints->client);
    sysmem_request_ = std::move(sysmem_endpoints->server);

    auto codec_endpoints = fidl::CreateEndpoints<fuchsia_media::StreamProcessor>();
    ZX_ASSERT(codec_endpoints.is_ok());
    codec_client_end_ = std::move(codec_endpoints->client);
    codec_request_ = std::move(codec_endpoints->server);

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
    if (codec_client_.is_valid()) {
      codec_client_->Sync().Then(
          [this](fidl::Result<fuchsia_media::StreamProcessor::Sync>& result) {
            if (result.is_ok()) {
              ++sync_completion_count_;
              StartSyncChain();
            }
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
  // ClientEnd while those that do intend to process received messages
  // use fidl::Client.
  fidl::ClientEnd<fuchsia_media::StreamProcessor> codec_client_end_;
  fidl::Client<fuchsia_media::StreamProcessor> codec_client_;

  fidl::ServerEnd<fuchsia_media::StreamProcessor> codec_request_;

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
  codec_client_end_.reset();
  RunLoopUntil([this] { return error_handler_ran_; });
  EXPECT_TRUE(error_handler_ran_);
}

TEST_F(CodecImplLifetime, CreateBindChannelCloseDeleteAsync) {
  Create(true, true);
  // Close the client end of the StreamProcessor channel.
  codec_client_end_.reset();
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

    codec_client_ =
        fidl::Client<fuchsia_media::StreamProcessor>(std::move(codec_client_end_), dispatcher());
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
  fuchsia_media::FormatDetails format_details;
  format_details.format_details_version_ordinal() = 1;
  format_details.mime_type() = "video/raw";

  fuchsia_media::VideoUncompressedFormat uncompressed;
  uncompressed.image_format().pixel_format().type() = fuchsia_sysmem::PixelFormatType::kNv12;
  uncompressed.image_format().coded_width() = 1920;
  uncompressed.image_format().coded_height() = 1088;
  uncompressed.image_format().bytes_per_row() = 1920;
  uncompressed.image_format().display_width() = 1920;
  uncompressed.image_format().display_height() = 1080;
  uncompressed.image_format().layers() = 1;
  uncompressed.image_format().color_space().type() = fuchsia_sysmem::ColorSpaceType::kRec709;
  uncompressed.image_format().has_pixel_aspect_ratio() = true;
  uncompressed.image_format().pixel_aspect_ratio_width() = 4;
  uncompressed.image_format().pixel_aspect_ratio_height() = 3;
  format_details.domain() = fuchsia_media::DomainFormat::WithVideo(
      fuchsia_media::VideoFormat::WithUncompressed(std::move(uncompressed)));

  CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

  ASSERT_TRUE(format_details.image_format().has_value());
  const auto& v2 = *format_details.image_format();
  ASSERT_TRUE(v2.pixel_format().has_value());
  EXPECT_EQ(*v2.pixel_format(), fuchsia_images2::PixelFormat::kNv12);
  ASSERT_TRUE(v2.size().has_value());
  EXPECT_EQ(v2.size()->width(), 1920u);
  EXPECT_EQ(v2.size()->height(), 1088u);
  ASSERT_TRUE(v2.bytes_per_row().has_value());
  EXPECT_EQ(*v2.bytes_per_row(), 1920u);
  ASSERT_TRUE(v2.display_rect().has_value());
  EXPECT_EQ(v2.display_rect()->x(), 0u);
  EXPECT_EQ(v2.display_rect()->y(), 0u);
  EXPECT_EQ(v2.display_rect()->width(), 1920u);
  EXPECT_EQ(v2.display_rect()->height(), 1080u);
  ASSERT_TRUE(v2.color_space().has_value());
  EXPECT_EQ(*v2.color_space(), fuchsia_images2::ColorSpace::kRec709);
  ASSERT_TRUE(v2.pixel_aspect_ratio().has_value());
  EXPECT_EQ(v2.pixel_aspect_ratio()->width(), 4u);
  EXPECT_EQ(v2.pixel_aspect_ratio()->height(), 3u);

  // Bridging V1 -> V2 also normalizes `domain.video.uncompressed` and is strictly idempotent.
  EXPECT_EQ(format_details.domain()->video()->uncompressed()->fourcc(),
            make_fourcc('N', 'V', '1', '2'));
  EXPECT_TRUE(format_details.domain()->video()->uncompressed()->planar());
  EXPECT_EQ(format_details.domain()->video()->uncompressed()->primary_width_pixels(), 1920u);
  EXPECT_EQ(format_details.domain()->video()->uncompressed()->primary_height_pixels(), 1088u);
  fuchsia_media::FormatDetails bridged_once = format_details;
  CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
  EXPECT_EQ(bridged_once, format_details);

  // When V1 omits both display_width/height and primary_display_width/height_pixels,
  // `display_rect` must be unset rather than `{0, 0, 0, 0}`.
  {
    fuchsia_media::FormatDetails no_display_details;
    no_display_details.format_details_version_ordinal() = 2;
    fuchsia_media::VideoUncompressedFormat no_disp_v1;
    no_disp_v1.image_format().pixel_format().type() = fuchsia_sysmem::PixelFormatType::kNv12;
    no_disp_v1.image_format().coded_width() = 1280;
    no_disp_v1.image_format().coded_height() = 720;
    no_disp_v1.image_format().bytes_per_row() = 1280;
    no_disp_v1.image_format().color_space().type() = fuchsia_sysmem::ColorSpaceType::kRec709;
    no_display_details.domain() = fuchsia_media::DomainFormat::WithVideo(
        fuchsia_media::VideoFormat::WithUncompressed(std::move(no_disp_v1)));

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&no_display_details);
    ASSERT_TRUE(no_display_details.image_format().has_value());
    EXPECT_FALSE(no_display_details.image_format()->display_rect().has_value());
  }

  // Bridging when `pixel_format` is omitted (`kInvalid`) in V1 -> V2 and V2 -> V1.
  {
    fuchsia_media::FormatDetails v1_no_pf;
    v1_no_pf.format_details_version_ordinal() = 3;
    fuchsia_media::VideoUncompressedFormat uncompressed_no_pf;
    uncompressed_no_pf.image_format().coded_width() = 640;
    uncompressed_no_pf.image_format().coded_height() = 480;
    uncompressed_no_pf.image_format().display_width() = 640;
    uncompressed_no_pf.image_format().display_height() = 480;
    uncompressed_no_pf.image_format().color_space().type() =
        fuchsia_sysmem::ColorSpaceType::kRec709;
    v1_no_pf.domain() = fuchsia_media::DomainFormat::WithVideo(
        fuchsia_media::VideoFormat::WithUncompressed(std::move(uncompressed_no_pf)));

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&v1_no_pf);
    ASSERT_TRUE(v1_no_pf.image_format().has_value());
    EXPECT_FALSE(v1_no_pf.image_format()->pixel_format().has_value());
    ASSERT_TRUE(v1_no_pf.image_format()->size().has_value());
    EXPECT_EQ(v1_no_pf.image_format()->size()->width(), 640u);
    EXPECT_EQ(v1_no_pf.image_format()->size()->height(), 480u);
    ASSERT_TRUE(v1_no_pf.image_format()->display_rect().has_value());
    EXPECT_EQ(v1_no_pf.image_format()->display_rect()->width(), 640u);
    EXPECT_EQ(v1_no_pf.image_format()->display_rect()->height(), 480u);

    fuchsia_media::FormatDetails v2_no_pf;
    v2_no_pf.format_details_version_ordinal() = 4;
    auto& img_no_pf = v2_no_pf.image_format().emplace();
    img_no_pf.size() = fuchsia_math::SizeU(640, 480);
    img_no_pf.display_rect() = fuchsia_math::RectU(0, 0, 640, 480);

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&v2_no_pf);
    ASSERT_TRUE(v2_no_pf.domain().has_value());
    ASSERT_EQ(v2_no_pf.domain()->Which(), fuchsia_media::DomainFormat::Tag::kVideo);
    ASSERT_EQ(v2_no_pf.domain()->video()->Which(), fuchsia_media::VideoFormat::Tag::kUncompressed);
    EXPECT_EQ(v2_no_pf.domain()->video()->uncompressed()->image_format().pixel_format().type(),
              fuchsia_sysmem::PixelFormatType::kInvalid);
    EXPECT_EQ(v2_no_pf.domain()->video()->uncompressed()->image_format().coded_width(), 640u);
    EXPECT_EQ(v2_no_pf.domain()->video()->uncompressed()->image_format().coded_height(), 480u);
  }
}

TEST(CodecImplFormatDetailsBridge, VideoV2ToV1Nv12AndYv12) {
  {
    fuchsia_media::FormatDetails format_details;
    format_details.format_details_version_ordinal() = 1;
    format_details.mime_type() = "video/raw";

    auto& v2 = format_details.image_format().emplace();
    v2.pixel_format() = fuchsia_images2::PixelFormat::kNv12;
    v2.size() = fuchsia_math::SizeU(1280, 736);
    v2.bytes_per_row() = 1280;
    v2.display_rect() = fuchsia_math::RectU(0, 0, 1280, 720);
    v2.color_space() = fuchsia_images2::ColorSpace::kRec709;
    v2.pixel_aspect_ratio() = fuchsia_math::SizeU(16, 9);

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.domain().has_value());
    ASSERT_EQ(format_details.domain()->Which(), fuchsia_media::DomainFormat::Tag::kVideo);
    ASSERT_EQ(format_details.domain()->video()->Which(),
              fuchsia_media::VideoFormat::Tag::kUncompressed);
    const auto& v1 = format_details.domain()->video()->uncompressed().value();
    EXPECT_EQ(v1.fourcc(), make_fourcc('N', 'V', '1', '2'));
    EXPECT_TRUE(v1.planar());
    EXPECT_FALSE(v1.swizzled());
    EXPECT_EQ(v1.primary_width_pixels(), 1280u);
    EXPECT_EQ(v1.primary_height_pixels(), 736u);
    EXPECT_EQ(v1.secondary_width_pixels(), 640u);
    EXPECT_EQ(v1.secondary_height_pixels(), 368u);
    EXPECT_EQ(v1.primary_line_stride_bytes(), 1280u);
    EXPECT_EQ(v1.secondary_line_stride_bytes(), 1280u);
    EXPECT_EQ(v1.primary_start_offset(), 0u);
    EXPECT_EQ(v1.secondary_start_offset(), 1280u * 736u);
    EXPECT_EQ(v1.tertiary_start_offset(), 1280u * 736u + 1u);
    EXPECT_EQ(v1.primary_pixel_stride(), 1u);
    EXPECT_EQ(v1.secondary_pixel_stride(), 2u);
    EXPECT_EQ(v1.primary_display_width_pixels(), 1280u);
    EXPECT_EQ(v1.primary_display_height_pixels(), 720u);
    EXPECT_TRUE(v1.has_pixel_aspect_ratio());
    EXPECT_EQ(v1.pixel_aspect_ratio_width(), 16u);
    EXPECT_EQ(v1.pixel_aspect_ratio_height(), 9u);
    EXPECT_EQ(v1.image_format().pixel_format().type(), fuchsia_sysmem::PixelFormatType::kNv12);
    EXPECT_EQ(v1.image_format().coded_width(), 1280u);
    EXPECT_EQ(v1.image_format().coded_height(), 736u);
    EXPECT_EQ(v1.image_format().bytes_per_row(), 1280u);
    EXPECT_EQ(v1.image_format().display_width(), 1280u);
    EXPECT_EQ(v1.image_format().display_height(), 720u);
  }

  {
    fuchsia_media::FormatDetails format_details;
    format_details.format_details_version_ordinal() = 2;
    format_details.mime_type() = "video/raw";

    auto& v2 = format_details.image_format().emplace();
    v2.pixel_format() = fuchsia_images2::PixelFormat::kYv12;
    v2.size() = fuchsia_math::SizeU(640, 368);
    v2.bytes_per_row() = 640;
    v2.display_rect() = fuchsia_math::RectU(0, 0, 640, 360);
    v2.color_space() = fuchsia_images2::ColorSpace::kRec709;

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.domain().has_value());
    const auto& v1 = format_details.domain()->video()->uncompressed().value();
    EXPECT_EQ(v1.fourcc(), make_fourcc('Y', 'V', '1', '2'));
    EXPECT_TRUE(v1.planar());
    EXPECT_FALSE(v1.swizzled());
    EXPECT_EQ(v1.primary_width_pixels(), 640u);
    EXPECT_EQ(v1.primary_height_pixels(), 368u);
    EXPECT_EQ(v1.secondary_width_pixels(), 320u);
    EXPECT_EQ(v1.secondary_height_pixels(), 184u);
    EXPECT_EQ(v1.primary_line_stride_bytes(), 640u);
    EXPECT_EQ(v1.secondary_line_stride_bytes(), 320u);
    EXPECT_EQ(v1.primary_start_offset(), 0u);
    EXPECT_EQ(v1.secondary_start_offset(), 640u * 368u);
    EXPECT_EQ(v1.tertiary_start_offset(), 640u * 368u + 320u * 184u);
    EXPECT_EQ(v1.primary_pixel_stride(), 1u);
    EXPECT_EQ(v1.secondary_pixel_stride(), 1u);
    EXPECT_EQ(v1.primary_display_width_pixels(), 640u);
    EXPECT_EQ(v1.primary_display_height_pixels(), 360u);
    EXPECT_FALSE(v1.has_pixel_aspect_ratio());

    // Mutating `image_format` on an already-bridged `FormatDetails` updates `domain`.
    format_details.image_format()->size() = fuchsia_math::SizeU(1920, 1088);
    format_details.image_format()->bytes_per_row() = 1920;
    format_details.image_format()->display_rect() = fuchsia_math::RectU(0, 0, 1920, 1080);
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
    EXPECT_EQ(format_details.domain()->video()->uncompressed()->primary_width_pixels(), 1920u);
    EXPECT_EQ(format_details.domain()->video()->uncompressed()->primary_display_height_pixels(),
              1080u);
  }

  {
    // Intel Y-tiled NV12 bridges `swizzled = true` and tile-aligned `secondary_start_offset`.
    fuchsia_media::FormatDetails format_details;
    format_details.format_details_version_ordinal() = 5;
    format_details.mime_type() = "video/raw";

    auto& v2 = format_details.image_format().emplace();
    v2.pixel_format() = fuchsia_images2::PixelFormat::kNv12;
    v2.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kIntelI915YTiled;
    v2.size() = fuchsia_math::SizeU(1920, 1080);
    v2.bytes_per_row() = 1920;
    v2.display_rect() = fuchsia_math::RectU(0, 0, 1920, 1080);
    v2.color_space() = fuchsia_images2::ColorSpace::kRec709;

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.domain().has_value());
    ASSERT_EQ(format_details.domain()->Which(), fuchsia_media::DomainFormat::Tag::kVideo);
    ASSERT_EQ(format_details.domain()->video()->Which(),
              fuchsia_media::VideoFormat::Tag::kUncompressed);
    const auto& v1 = format_details.domain()->video()->uncompressed().value();
    EXPECT_EQ(v1.fourcc(), make_fourcc('N', 'V', '1', '2'));
    EXPECT_TRUE(v1.planar());
    EXPECT_TRUE(v1.swizzled());
    EXPECT_EQ(v1.primary_line_stride_bytes(), 1920u);
    EXPECT_EQ(v1.secondary_line_stride_bytes(), 1920u);
    EXPECT_EQ(v1.secondary_start_offset(), 1920u * 1088u);
    EXPECT_EQ(v1.tertiary_start_offset(), (1920u * 1088u) + 1u);
    EXPECT_EQ(v1.image_format().pixel_format().has_format_modifier(), true);
    EXPECT_EQ(v1.image_format().pixel_format().format_modifier().value(),
              fuchsia_sysmem::kFormatModifierIntelI915YTiled);
  }

  {
    // R8G8B8A8 bridges with `FakeCodecAdapter`'s constants (256x144, stride 1024).
    fuchsia_media::FormatDetails format_details;
    format_details.format_details_version_ordinal() = 6;
    format_details.mime_type() = "video/raw";

    auto& v2 = format_details.image_format().emplace();
    v2.pixel_format() = fuchsia_images2::PixelFormat::kR8G8B8A8;
    v2.color_space() = fuchsia_images2::ColorSpace::kSrgb;
    v2.size() = fuchsia_math::SizeU(256, 144);
    v2.bytes_per_row() = 1024;
    v2.display_rect() = fuchsia_math::RectU(0, 0, 256, 144);

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.domain().has_value());
    ASSERT_EQ(format_details.domain()->Which(), fuchsia_media::DomainFormat::Tag::kVideo);
    ASSERT_EQ(format_details.domain()->video()->Which(),
              fuchsia_media::VideoFormat::Tag::kUncompressed);
    const auto& v1 = format_details.domain()->video()->uncompressed().value();
    EXPECT_EQ(v1.fourcc(), make_fourcc('R', 'G', 'B', 'A'));
    EXPECT_FALSE(v1.planar());
    EXPECT_FALSE(v1.swizzled());
    EXPECT_EQ(v1.primary_width_pixels(), 256u);
    EXPECT_EQ(v1.primary_height_pixels(), 144u);
    EXPECT_EQ(v1.primary_pixel_stride(), 4u);
    EXPECT_EQ(v1.primary_line_stride_bytes(), 1024u);
    EXPECT_EQ(v1.primary_display_width_pixels(), 256u);
    EXPECT_EQ(v1.primary_display_height_pixels(), 144u);
  }

  {
    // Overflowing dimensions must not panic or wrap around.
    fuchsia_media::FormatDetails overflow_details;
    overflow_details.format_details_version_ordinal() = 3;
    auto& v2 = overflow_details.image_format().emplace();
    v2.pixel_format() = fuchsia_images2::PixelFormat::kNv12;
    v2.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kIntelI915YTiled;
    v2.size() = fuchsia_math::SizeU(65536, 0xFFFFFFF0u);
    v2.bytes_per_row() = 65536;

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&overflow_details);
    EXPECT_FALSE(overflow_details.domain().has_value());
  }

  {
    // Compressed video `mime_type` synthesizes `domain.video.compressed`.
    fuchsia_media::FormatDetails compressed_video;
    compressed_video.format_details_version_ordinal() = 4;
    compressed_video.mime_type() = "video/h264";

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&compressed_video);
    ASSERT_TRUE(compressed_video.domain().has_value());
    ASSERT_EQ(compressed_video.domain()->Which(), fuchsia_media::DomainFormat::Tag::kVideo);
    EXPECT_EQ(compressed_video.domain()->video()->Which(),
              fuchsia_media::VideoFormat::Tag::kCompressed);
  }

  {
    // Linear NV12 and I420 `ImageFormat` omitting optional `bytes_per_row` (and omitting
    // `display_rect` while setting `valid_size`) bridges to unpadded V1 strides and non-overlapping
    // UV plane offsets.
    fuchsia_media::FormatDetails nv12_unpadded;
    nv12_unpadded.format_details_version_ordinal() = 5;
    nv12_unpadded.mime_type() = "video/raw";
    auto& nv12_v2 = nv12_unpadded.image_format().emplace();
    nv12_v2.pixel_format() = fuchsia_images2::PixelFormat::kNv12;
    nv12_v2.size() = fuchsia_math::SizeU(640, 368);
    nv12_v2.valid_size() = fuchsia_math::SizeU(640, 360);

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&nv12_unpadded);
    ASSERT_TRUE(nv12_unpadded.domain().has_value());
    const auto& nv12_v1 = nv12_unpadded.domain()->video()->uncompressed().value();
    EXPECT_FALSE(nv12_unpadded.image_format()->bytes_per_row().has_value());
    EXPECT_EQ(nv12_v1.image_format().bytes_per_row(), 640u);
    EXPECT_EQ(nv12_v1.primary_line_stride_bytes(), 640u);
    EXPECT_EQ(nv12_v1.secondary_line_stride_bytes(), 640u);
    EXPECT_EQ(nv12_v1.secondary_start_offset(), 640u * 368u);
    EXPECT_EQ(nv12_v1.tertiary_start_offset(), 640u * 368u + 1u);
    EXPECT_EQ(nv12_v1.primary_display_width_pixels(), 640u);
    EXPECT_EQ(nv12_v1.primary_display_height_pixels(), 360u);

    fuchsia_media::FormatDetails nv12_bridged_once = nv12_unpadded;
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&nv12_unpadded);
    EXPECT_EQ(nv12_bridged_once, nv12_unpadded);

    fuchsia_media::FormatDetails i420_unpadded;
    i420_unpadded.format_details_version_ordinal() = 6;
    i420_unpadded.mime_type() = "video/raw";
    auto& i420_v2 = i420_unpadded.image_format().emplace();
    i420_v2.pixel_format() = fuchsia_images2::PixelFormat::kI420;
    i420_v2.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
    i420_v2.size() = fuchsia_math::SizeU(640, 368);

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&i420_unpadded);
    ASSERT_TRUE(i420_unpadded.domain().has_value());
    const auto& i420_v1 = i420_unpadded.domain()->video()->uncompressed().value();
    EXPECT_EQ(i420_v1.primary_line_stride_bytes(), 640u);
    EXPECT_EQ(i420_v1.secondary_line_stride_bytes(), 320u);
    EXPECT_EQ(i420_v1.secondary_start_offset(), 640u * 368u);
    EXPECT_EQ(i420_v1.tertiary_start_offset(), 640u * 368u + 320u * 184u);
  }
}

TEST(CodecImplFormatDetailsBridge, AudioV1ToV2AndV2ToV1) {
  {
    fuchsia_media::FormatDetails format_details;
    format_details.format_details_version_ordinal() = 1;
    format_details.mime_type() = "audio/raw";

    fuchsia_media::PcmFormat pcm_v1;
    pcm_v1.pcm_mode() = fuchsia_media::AudioPcmMode::kLinear;
    pcm_v1.bits_per_sample() = 16;
    pcm_v1.frames_per_second() = 48000;
    pcm_v1.channel_map() = {
        fuchsia_media::AudioChannelId::kLf, fuchsia_media::AudioChannelId::kRf,
        fuchsia_media::AudioChannelId::kCf, fuchsia_media::AudioChannelId::kLfe,
        fuchsia_media::AudioChannelId::kLr, fuchsia_media::AudioChannelId::kRr,
        fuchsia_media::AudioChannelId::kLs, fuchsia_media::AudioChannelId::kRs,
        fuchsia_media::AudioChannelId::kCs,
    };
    format_details.domain() =
        fuchsia_media::DomainFormat::WithAudio(fuchsia_media::AudioFormat::WithUncompressed(
            fuchsia_media::AudioUncompressedFormat::WithPcm(std::move(pcm_v1))));

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.pcm_format().has_value());
    const auto& pcm_v2 = *format_details.pcm_format();
    ASSERT_TRUE(pcm_v2.pcm_mode().has_value());
    EXPECT_EQ(*pcm_v2.pcm_mode(), fuchsia_media::AudioPcmMode2::kLinear);
    ASSERT_TRUE(pcm_v2.bits_per_sample().has_value());
    EXPECT_EQ(*pcm_v2.bits_per_sample(), 16u);
    ASSERT_TRUE(pcm_v2.frames_per_second().has_value());
    EXPECT_EQ(*pcm_v2.frames_per_second(), 48000u);
    ASSERT_TRUE(pcm_v2.channel_map().has_value());
    std::vector<fuchsia_media::AudioChannelId2> expected_v2 = {
        fuchsia_media::AudioChannelId2::kFl, fuchsia_media::AudioChannelId2::kFr,
        fuchsia_media::AudioChannelId2::kFc, fuchsia_media::AudioChannelId2::kLfe,
        fuchsia_media::AudioChannelId2::kBl, fuchsia_media::AudioChannelId2::kBr,
        fuchsia_media::AudioChannelId2::kSl, fuchsia_media::AudioChannelId2::kSr,
        fuchsia_media::AudioChannelId2::kBc,
    };
    EXPECT_EQ(*pcm_v2.channel_map(), expected_v2);
    ExpectFormatDetailsFidlValid(format_details);
  }

  {
    fuchsia_media::FormatDetails format_details;
    format_details.format_details_version_ordinal() = 2;
    format_details.mime_type() = "audio/raw";

    auto& pcm_v2 = format_details.pcm_format().emplace();
    pcm_v2.pcm_mode() = fuchsia_media::AudioPcmMode2::kLinear;
    pcm_v2.bits_per_sample() = 24;
    pcm_v2.frames_per_second() = 96000;
    pcm_v2.channel_map() = {
        fuchsia_media::AudioChannelId2::kFl,
        fuchsia_media::AudioChannelId2::kFr,
        fuchsia_media::AudioChannelId2::kBc,
    };

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
    ExpectFormatDetailsFidlValid(format_details);

    ASSERT_TRUE(format_details.domain().has_value());
    ASSERT_EQ(format_details.domain()->Which(), fuchsia_media::DomainFormat::Tag::kAudio);
    ASSERT_EQ(format_details.domain()->audio()->Which(),
              fuchsia_media::AudioFormat::Tag::kUncompressed);
    ASSERT_EQ(format_details.domain()->audio()->uncompressed()->Which(),
              fuchsia_media::AudioUncompressedFormat::Tag::kPcm);
    const auto& pcm_v1 = format_details.domain()->audio()->uncompressed()->pcm().value();
    EXPECT_EQ(pcm_v1.pcm_mode(), fuchsia_media::AudioPcmMode::kLinear);
    EXPECT_EQ(pcm_v1.bits_per_sample(), 24u);
    EXPECT_EQ(pcm_v1.frames_per_second(), 96000u);
    std::vector<fuchsia_media::AudioChannelId> expected_v1 = {
        fuchsia_media::AudioChannelId::kLf,
        fuchsia_media::AudioChannelId::kRf,
        fuchsia_media::AudioChannelId::kCs,
    };
    EXPECT_EQ(pcm_v1.channel_map(), expected_v1);

    // Mutating `pcm_format` on an already-bridged `FormatDetails` updates `domain`.
    format_details.pcm_format()->frames_per_second() = 44100;
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
    ExpectFormatDetailsFidlValid(format_details);
    EXPECT_EQ(format_details.domain()->audio()->uncompressed()->pcm()->frames_per_second(), 44100u);
  }

  {
    // Spatial channels (e.g. TFL) that cannot be represented in legacy AudioChannelId
    // should not synthesize a legacy PcmFormat.
    fuchsia_media::FormatDetails format_details;
    format_details.format_details_version_ordinal() = 3;
    format_details.mime_type() = "audio/raw";

    auto& pcm_v2 = format_details.pcm_format().emplace();
    pcm_v2.pcm_mode() = fuchsia_media::AudioPcmMode2::kLinear;
    pcm_v2.bits_per_sample() = 16;
    pcm_v2.frames_per_second() = 48000;
    pcm_v2.channel_map() = {
        fuchsia_media::AudioChannelId2::kFl,
        fuchsia_media::AudioChannelId2::kFr,
        fuchsia_media::AudioChannelId2::kTfl,
    };

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
    EXPECT_FALSE(format_details.domain().has_value());
    ExpectFormatDetailsFidlValid(format_details);
  }

  {
    // Extended V2 channel IDs `[kMinExtendedAudioChannelId2, kMaxAudioChannelId2]` are
    // V2-only and must not synthesize a legacy `PcmFormat` (which uses a `strict enum` that rejects
    // unlisted extended values on FIDL wire validation).
    for (uint32_t raw_id : {
             fuchsia_media::kMinExtendedAudioChannelId2,
             fuchsia_media::kMinExtendedAudioChannelId2 + 1,
             fuchsia_media::kMaxAudioChannelId2 - 1,
             fuchsia_media::kMaxAudioChannelId2,
         }) {
      fuchsia_media::FormatDetails format_details;
      format_details.format_details_version_ordinal() = 3;
      format_details.mime_type() = "audio/raw";

      auto& pcm_v2 = format_details.pcm_format().emplace();
      pcm_v2.pcm_mode() = fuchsia_media::AudioPcmMode2::kLinear;
      pcm_v2.bits_per_sample() = 16;
      pcm_v2.frames_per_second() = 48000;
      pcm_v2.channel_map() = {
          fuchsia_media::AudioChannelId2::kFl,
          fuchsia_media::AudioChannelId2(raw_id),
      };

      CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
      EXPECT_FALSE(format_details.domain().has_value()) << "raw_id=0x" << std::hex << raw_id;
      EXPECT_TRUE(format_details.pcm_format().has_value()) << "raw_id=0x" << std::hex << raw_id;
      ExpectFormatDetailsFidlValid(format_details);
    }
  }

  {
    // Non-role legacy `AudioChannelId` range markers (`kEndDefined`, `kExtendedChannelIdBase`,
    // `kMax`) and unlisted values do not populate `pcm_format`.
    for (fuchsia_media::AudioChannelId sentinel : {
             fuchsia_media::AudioChannelId::kEndDefined,
             fuchsia_media::AudioChannelId::kExtendedChannelIdBase,
             fuchsia_media::AudioChannelId::kMax,
         }) {
      fuchsia_media::FormatDetails format_details;
      format_details.format_details_version_ordinal() = 3;
      format_details.mime_type() = "audio/raw";
      fuchsia_media::PcmFormat pcm_v1;
      pcm_v1.pcm_mode() = fuchsia_media::AudioPcmMode::kLinear;
      pcm_v1.bits_per_sample() = 16;
      pcm_v1.frames_per_second() = 48000;
      pcm_v1.channel_map() = {sentinel};
      format_details.domain() =
          fuchsia_media::DomainFormat::WithAudio(fuchsia_media::AudioFormat::WithUncompressed(
              fuchsia_media::AudioUncompressedFormat::WithPcm(std::move(pcm_v1))));

      CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
      EXPECT_FALSE(format_details.pcm_format().has_value());
      ExpectFormatDetailsFidlValid(format_details);
    }

    fuchsia_media::FormatDetails v1_unlisted;
    v1_unlisted.format_details_version_ordinal() = 3;
    v1_unlisted.mime_type() = "audio/raw";
    fuchsia_media::PcmFormat pcm_v1;
    pcm_v1.pcm_mode() = fuchsia_media::AudioPcmMode::kLinear;
    pcm_v1.bits_per_sample() = 16;
    pcm_v1.frames_per_second() = 48000;
    pcm_v1.channel_map() = {
        static_cast<fuchsia_media::AudioChannelId>(fuchsia_media::kMinExtendedAudioChannelId2 + 1),
    };
    v1_unlisted.domain() =
        fuchsia_media::DomainFormat::WithAudio(fuchsia_media::AudioFormat::WithUncompressed(
            fuchsia_media::AudioUncompressedFormat::WithPcm(std::move(pcm_v1))));
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&v1_unlisted);
    EXPECT_FALSE(v1_unlisted.pcm_format().has_value());
  }

  {
    // Compressed audio `mime_type` synthesizes `domain.audio.compressed`.
    fuchsia_media::FormatDetails compressed_audio;
    compressed_audio.format_details_version_ordinal() = 4;
    compressed_audio.mime_type() = "audio/aac";

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&compressed_audio);
    ASSERT_TRUE(compressed_audio.domain().has_value());
    ASSERT_EQ(compressed_audio.domain()->Which(), fuchsia_media::DomainFormat::Tag::kAudio);
    EXPECT_EQ(compressed_audio.domain()->audio()->Which(),
              fuchsia_media::AudioFormat::Tag::kCompressed);
    ExpectFormatDetailsFidlValid(compressed_audio);
  }

  {
    // Legacy V1 24-bit `PcmFormat` paired with `encoder_settings.lc3` bridges to explicit
    // `bytes_per_sample == 4` and stays idempotent on repeated calls.
    fuchsia_media::FormatDetails lc3_v1;
    lc3_v1.format_details_version_ordinal() = 5;
    lc3_v1.mime_type() = "audio/raw";
    lc3_v1.encoder_settings() = fuchsia_media::EncoderSettings::WithLc3({});
    fuchsia_media::PcmFormat pcm_v1;
    pcm_v1.pcm_mode() = fuchsia_media::AudioPcmMode::kLinear;
    pcm_v1.bits_per_sample() = 24;
    pcm_v1.frames_per_second() = 48000;
    pcm_v1.channel_map() = {fuchsia_media::AudioChannelId::kLf};
    lc3_v1.domain() =
        fuchsia_media::DomainFormat::WithAudio(fuchsia_media::AudioFormat::WithUncompressed(
            fuchsia_media::AudioUncompressedFormat::WithPcm(std::move(pcm_v1))));

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&lc3_v1);
    ASSERT_TRUE(lc3_v1.pcm_format().has_value());
    ASSERT_TRUE(lc3_v1.pcm_format()->bytes_per_sample().has_value());
    EXPECT_EQ(*lc3_v1.pcm_format()->bytes_per_sample(), 4u);
    EXPECT_EQ((*lc3_v1.pcm_format()->channel_map())[0], fuchsia_media::AudioChannelId2::kFl);
    ASSERT_TRUE(lc3_v1.domain().has_value());
    ExpectFormatDetailsFidlValid(lc3_v1);

    fuchsia_media::FormatDetails bridged_once = lc3_v1;
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&lc3_v1);
    EXPECT_EQ(bridged_once, lc3_v1);

    // Packed 24-bit LC3 (`bytes_per_sample == 3` or omitted) cannot be represented in legacy V1
    // `PcmFormat` (where 24-bit LC3 implied 4-byte containers), so `domain` must be cleared.
    fuchsia_media::FormatDetails lc3_packed_explicit = lc3_v1;
    lc3_packed_explicit.pcm_format()->bytes_per_sample() = 3;
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&lc3_packed_explicit);
    EXPECT_FALSE(lc3_packed_explicit.domain().has_value());
    EXPECT_TRUE(lc3_packed_explicit.pcm_format().has_value());
    ExpectFormatDetailsFidlValid(lc3_packed_explicit);

    fuchsia_media::FormatDetails lc3_packed_implicit = lc3_v1;
    lc3_packed_implicit.pcm_format()->bytes_per_sample().reset();
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&lc3_packed_implicit);
    EXPECT_FALSE(lc3_packed_implicit.domain().has_value());
    EXPECT_TRUE(lc3_packed_implicit.pcm_format().has_value());
    ExpectFormatDetailsFidlValid(lc3_packed_implicit);
  }

  {
    // Oversized in-process V1 `PcmFormat.channel_map` (> 16 channels) must not populate
    // `pcm_format`.
    fuchsia_media::FormatDetails oversized_v1;
    oversized_v1.format_details_version_ordinal() = 6;
    oversized_v1.mime_type() = "audio/raw";
    fuchsia_media::PcmFormat pcm_v1;
    pcm_v1.pcm_mode() = fuchsia_media::AudioPcmMode::kLinear;
    pcm_v1.bits_per_sample() = 16;
    pcm_v1.frames_per_second() = 48000;
    pcm_v1.channel_map().assign(17, fuchsia_media::AudioChannelId::kSkip);
    oversized_v1.domain() =
        fuchsia_media::DomainFormat::WithAudio(fuchsia_media::AudioFormat::WithUncompressed(
            fuchsia_media::AudioUncompressedFormat::WithPcm(std::move(pcm_v1))));

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&oversized_v1);
    EXPECT_FALSE(oversized_v1.pcm_format().has_value());
  }
}

TEST(CodecImplFormatDetailsBridge, CryptoV1ToV2AndV2ToV1) {
  {
    fuchsia_media::FormatDetails format_details;
    format_details.format_details_version_ordinal() = 1;
    fuchsia_media::EncryptedFormat enc_v1;
    enc_v1.scheme() = "cenc";
    enc_v1.key_id() = {1, 2, 3, 4};
    enc_v1.init_vector() = {5, 6, 7, 8};
    format_details.domain() = fuchsia_media::DomainFormat::WithCrypto(
        fuchsia_media::CryptoFormat::WithEncrypted(std::move(enc_v1)));

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.encrypted_format().has_value());
    const auto& enc_v2 = *format_details.encrypted_format();
    ASSERT_TRUE(enc_v2.scheme().has_value());
    EXPECT_EQ(*enc_v2.scheme(), "cenc");
    ASSERT_TRUE(enc_v2.key_id().has_value());
    EXPECT_EQ(*enc_v2.key_id(), (std::vector<uint8_t>{1, 2, 3, 4}));
    ASSERT_TRUE(enc_v2.init_vector().has_value());
    EXPECT_EQ(*enc_v2.init_vector(), (std::vector<uint8_t>{5, 6, 7, 8}));
  }

  {
    fuchsia_media::FormatDetails format_details;
    format_details.format_details_version_ordinal() = 2;
    auto& enc_v2 = format_details.encrypted_format().emplace();
    enc_v2.scheme() = "cbcs";
    enc_v2.key_id() = {9, 10, 11, 12};

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.domain().has_value());
    ASSERT_EQ(format_details.domain()->Which(), fuchsia_media::DomainFormat::Tag::kCrypto);
    ASSERT_EQ(format_details.domain()->crypto()->Which(),
              fuchsia_media::CryptoFormat::Tag::kEncrypted);
    const auto& enc_v1 = format_details.domain()->crypto()->encrypted().value();
    ASSERT_TRUE(enc_v1.scheme().has_value());
    EXPECT_EQ(*enc_v1.scheme(), "cbcs");
    ASSERT_TRUE(enc_v1.key_id().has_value());
    EXPECT_EQ(*enc_v1.key_id(), (std::vector<uint8_t>{9, 10, 11, 12}));

    // Mutating `encrypted_format` on an already-bridged `FormatDetails` updates `domain`.
    format_details.encrypted_format()->scheme() = "cenc";
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
    EXPECT_EQ(*format_details.domain()->crypto()->encrypted()->scheme(), "cenc");
  }
}

TEST(CodecImplFormatDetailsBridge, VideoV1ToV2OptionalFieldOmission) {
  {
    // Display-only V1 metadata with default/zero coded dimensions, stride, and INVALID color space
    // must leave `color_space`, `bytes_per_row`, `size`, and `valid_size` unset on V2
    // `image_format`.
    fuchsia_media::FormatDetails format_details;
    format_details.format_details_version_ordinal() = 1;
    format_details.mime_type() = "video/raw";

    fuchsia_media::VideoUncompressedFormat uncompressed;
    uncompressed.image_format().pixel_format().type() = fuchsia_sysmem::PixelFormatType::kNv12;
    uncompressed.image_format().coded_width() = 0;
    uncompressed.image_format().coded_height() = 0;
    uncompressed.image_format().bytes_per_row() = 0;
    uncompressed.image_format().display_width() = 640;
    uncompressed.image_format().display_height() = 480;
    uncompressed.image_format().color_space().type() = fuchsia_sysmem::ColorSpaceType::kInvalid;
    format_details.domain() = fuchsia_media::DomainFormat::WithVideo(
        fuchsia_media::VideoFormat::WithUncompressed(std::move(uncompressed)));

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.image_format().has_value());
    const auto& v2 = *format_details.image_format();
    ASSERT_TRUE(v2.pixel_format().has_value());
    EXPECT_EQ(*v2.pixel_format(), fuchsia_images2::PixelFormat::kNv12);
    EXPECT_FALSE(v2.color_space().has_value());
    EXPECT_FALSE(v2.bytes_per_row().has_value());
    EXPECT_FALSE(v2.size().has_value());
    EXPECT_FALSE(v2.valid_size().has_value());
    ASSERT_TRUE(v2.display_rect().has_value());
    EXPECT_EQ(v2.display_rect()->x(), 0u);
    EXPECT_EQ(v2.display_rect()->y(), 0u);
    EXPECT_EQ(v2.display_rect()->width(), 640u);
    EXPECT_EQ(v2.display_rect()->height(), 480u);

    fuchsia_media::FormatDetails bridged_once = format_details;
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);
    EXPECT_EQ(bridged_once, format_details);
    ExpectFormatDetailsFidlValid(format_details);
  }

  {
    // Legacy V1 encoder input omitting `pixel_format`, `color_space`, and `bytes_per_row` while
    // specifying coded dimensions leaves those optional V2 fields unset.
    fuchsia_media::FormatDetails format_details;
    format_details.format_details_version_ordinal() = 2;
    format_details.mime_type() = "video/raw";

    fuchsia_media::VideoUncompressedFormat uncompressed;
    uncompressed.image_format().coded_width() = 1280;
    uncompressed.image_format().coded_height() = 720;
    uncompressed.image_format().bytes_per_row() = 0;
    uncompressed.image_format().color_space().type() = fuchsia_sysmem::ColorSpaceType::kInvalid;
    format_details.domain() = fuchsia_media::DomainFormat::WithVideo(
        fuchsia_media::VideoFormat::WithUncompressed(std::move(uncompressed)));

    CodecImpl::EnsureFormatDetailsBridgedForTesting(&format_details);

    ASSERT_TRUE(format_details.image_format().has_value());
    const auto& v2 = *format_details.image_format();
    EXPECT_FALSE(v2.pixel_format().has_value());
    EXPECT_FALSE(v2.color_space().has_value());
    EXPECT_FALSE(v2.bytes_per_row().has_value());
    EXPECT_FALSE(v2.display_rect().has_value());
    ASSERT_TRUE(v2.size().has_value());
    EXPECT_EQ(v2.size()->width(), 1280u);
    EXPECT_EQ(v2.size()->height(), 720u);
    ExpectFormatDetailsFidlValid(format_details);
  }
}

TEST(CodecImplFormatDetailsBridge, ClearsStaleDomainOnV2OnlyMutation) {
  // Video: start from a bridged V2+V1 FormatDetails, then mutate `image_format` to V2-only values.
  {
    fuchsia_media::FormatDetails base_video;
    base_video.format_details_version_ordinal() = 1;
    base_video.mime_type() = "video/raw";
    auto& v2 = base_video.image_format().emplace();
    v2.pixel_format() = fuchsia_images2::PixelFormat::kNv12;
    v2.size() = fuchsia_math::SizeU(1280, 720);
    v2.bytes_per_row() = 1280;
    v2.display_rect() = fuchsia_math::RectU(0, 0, 1280, 720);
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&base_video);
    ASSERT_TRUE(base_video.domain().has_value());
    ExpectFormatDetailsFidlValid(base_video);

    // Non-zero `display_rect.x` clears stale `domain`.
    fuchsia_media::FormatDetails cropped_x = base_video;
    cropped_x.image_format()->display_rect() = fuchsia_math::RectU(4, 0, 1276, 720);
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&cropped_x);
    EXPECT_FALSE(cropped_x.domain().has_value());
    EXPECT_TRUE(cropped_x.image_format().has_value());
    ExpectFormatDetailsFidlValid(cropped_x);

    // Non-zero `display_rect.y` clears stale `domain`.
    fuchsia_media::FormatDetails cropped_y = base_video;
    cropped_y.image_format()->display_rect() = fuchsia_math::RectU(0, 2, 1280, 718);
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&cropped_y);
    EXPECT_FALSE(cropped_y.domain().has_value());
    ExpectFormatDetailsFidlValid(cropped_y);

    // V2-only `PixelFormat::kP010` clears stale `domain`.
    fuchsia_media::FormatDetails p010 = base_video;
    p010.image_format()->pixel_format() = fuchsia_images2::PixelFormat::kP010;
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&p010);
    EXPECT_FALSE(p010.domain().has_value());
    ExpectFormatDetailsFidlValid(p010);

    // V2-only `ColorSpace` clears stale `domain`.
    fuchsia_media::FormatDetails unknown_cs = base_video;
    unknown_cs.image_format()->color_space() = fuchsia_images2::ColorSpace(100);
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&unknown_cs);
    EXPECT_FALSE(unknown_cs.domain().has_value());
    ExpectFormatDetailsFidlValid(unknown_cs);
  }

  // Audio: start from a bridged V2+V1 FormatDetails, then mutate `pcm_format` to V2-only values.
  {
    fuchsia_media::FormatDetails base_audio;
    base_audio.format_details_version_ordinal() = 1;
    base_audio.mime_type() = "audio/raw";
    auto& pcm_v2 = base_audio.pcm_format().emplace();
    pcm_v2.pcm_mode() = fuchsia_media::AudioPcmMode2::kLinear;
    pcm_v2.bits_per_sample() = 24;
    pcm_v2.frames_per_second() = 48000;
    pcm_v2.channel_map() = {
        fuchsia_media::AudioChannelId2::kFl,
        fuchsia_media::AudioChannelId2::kFr,
        fuchsia_media::AudioChannelId2::kFc,
    };
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&base_audio);
    ASSERT_TRUE(base_audio.domain().has_value());
    ExpectFormatDetailsFidlValid(base_audio);

    // Packed 24-bit (`bytes_per_sample == 3`) remains representable in V1.
    fuchsia_media::FormatDetails packed_24 = base_audio;
    packed_24.pcm_format()->bytes_per_sample() = 3;
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&packed_24);
    EXPECT_TRUE(packed_24.domain().has_value());
    ExpectFormatDetailsFidlValid(packed_24);

    // 24-in-32 padded (`bits_per_sample == 24`, `bytes_per_sample == 4`) clears stale `domain`.
    fuchsia_media::FormatDetails padded_24_in_32 = base_audio;
    padded_24_in_32.pcm_format()->bytes_per_sample() = 4;
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&padded_24_in_32);
    EXPECT_FALSE(padded_24_in_32.domain().has_value());
    EXPECT_TRUE(padded_24_in_32.pcm_format().has_value());
    ExpectFormatDetailsFidlValid(padded_24_in_32);

    // Wide spatial channels (`kFwl` / `kFwr`) clear stale `domain`.
    fuchsia_media::FormatDetails wide_channels = base_audio;
    wide_channels.pcm_format()->channel_map() = {
        fuchsia_media::AudioChannelId2::kFl,
        fuchsia_media::AudioChannelId2::kFr,
        fuchsia_media::AudioChannelId2::kFwl,
        fuchsia_media::AudioChannelId2::kFwr,
    };
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&wide_channels);
    EXPECT_FALSE(wide_channels.domain().has_value());
    ExpectFormatDetailsFidlValid(wide_channels);

    // Extended channel IDs `[kMinExtendedAudioChannelId2, kMaxAudioChannelId2]` clear stale
    // `domain` and remain FIDL-valid.
    for (uint32_t raw_id : {
             fuchsia_media::kMinExtendedAudioChannelId2,
             fuchsia_media::kMinExtendedAudioChannelId2 + 1,
             fuchsia_media::kMaxAudioChannelId2,
         }) {
      fuchsia_media::FormatDetails extended_channel = base_audio;
      extended_channel.pcm_format()->channel_map() = {
          fuchsia_media::AudioChannelId2::kFl,
          fuchsia_media::AudioChannelId2(raw_id),
      };
      CodecImpl::EnsureFormatDetailsBridgedForTesting(&extended_channel);
      EXPECT_FALSE(extended_channel.domain().has_value()) << "raw_id=0x" << std::hex << raw_id;
      EXPECT_TRUE(extended_channel.pcm_format().has_value()) << "raw_id=0x" << std::hex << raw_id;
      ExpectFormatDetailsFidlValid(extended_channel);
    }

    // > 16 channels clears stale `domain`.
    fuchsia_media::FormatDetails seventeen_channels = base_audio;
    seventeen_channels.pcm_format()->channel_map() =
        std::vector<fuchsia_media::AudioChannelId2>(17, fuchsia_media::AudioChannelId2::kSkip);
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&seventeen_channels);
    EXPECT_FALSE(seventeen_channels.domain().has_value());
    ExpectFormatDetailsFidlValid(seventeen_channels);

    // `AudioPcmMode2::kFloat` clears stale `domain`.
    fuchsia_media::FormatDetails float_pcm = base_audio;
    float_pcm.pcm_format()->pcm_mode() = fuchsia_media::AudioPcmMode2::kFloat;
    float_pcm.pcm_format()->bits_per_sample() = 32;
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&float_pcm);
    EXPECT_FALSE(float_pcm.domain().has_value());
    ExpectFormatDetailsFidlValid(float_pcm);

    // Missing required field clears stale `domain`.
    fuchsia_media::FormatDetails missing_fps = base_audio;
    missing_fps.pcm_format()->frames_per_second().reset();
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&missing_fps);
    EXPECT_FALSE(missing_fps.domain().has_value());
    ExpectFormatDetailsFidlValid(missing_fps);

    // `bits_per_sample == 0` clears stale `domain`.
    fuchsia_media::FormatDetails zero_bits = base_audio;
    zero_bits.pcm_format()->bits_per_sample() = 0;
    CodecImpl::EnsureFormatDetailsBridgedForTesting(&zero_bits);
    EXPECT_FALSE(zero_bits.domain().has_value());
    ExpectFormatDetailsFidlValid(zero_bits);
  }
}

TEST(CodecImplFormatDetailsBridge, InputNormalizationMovesAndClearsDomain) {
  // Crypto: V1 `domain.crypto.encrypted` moves buffers into `encrypted_format` and clears `domain`,
  // matching an equivalent V2 `FormatDetails` via `operator==`.
  {
    fuchsia_media::FormatDetails v1_crypto;
    v1_crypto.format_details_version_ordinal() = 1;
    fuchsia_media::EncryptedFormat enc_v1;
    enc_v1.scheme() = "cenc";
    enc_v1.key_id() = {1, 2, 3, 4};
    enc_v1.init_vector() = {5, 6, 7, 8};
    enc_v1.subsamples() =
        std::vector<fuchsia_media::SubsampleEntry>{fuchsia_media::SubsampleEntry(16, 48)};
    const uint8_t* key_id_ptr = enc_v1.key_id()->data();
    const uint8_t* iv_ptr = enc_v1.init_vector()->data();
    v1_crypto.domain() = fuchsia_media::DomainFormat::WithCrypto(
        fuchsia_media::CryptoFormat::WithEncrypted(std::move(enc_v1)));

    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&v1_crypto);
    EXPECT_FALSE(v1_crypto.domain().has_value());
    ASSERT_TRUE(v1_crypto.encrypted_format().has_value());
    EXPECT_EQ(v1_crypto.encrypted_format()->key_id()->data(), key_id_ptr);
    EXPECT_EQ(v1_crypto.encrypted_format()->init_vector()->data(), iv_ptr);

    fuchsia_media::FormatDetails v2_crypto;
    v2_crypto.format_details_version_ordinal() = 1;
    auto& enc_v2 = v2_crypto.encrypted_format().emplace();
    enc_v2.scheme() = "cenc";
    enc_v2.key_id() = {1, 2, 3, 4};
    enc_v2.init_vector() = {5, 6, 7, 8};
    enc_v2.subsamples() =
        std::vector<fuchsia_media::SubsampleEntry>{fuchsia_media::SubsampleEntry(16, 48)};

    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&v2_crypto);
    EXPECT_FALSE(v2_crypto.domain().has_value());
    ASSERT_TRUE(v2_crypto.encrypted_format().has_value());
    EXPECT_EQ(v1_crypto, v2_crypto);
  }

  // Video and audio: input normalization preserves bidirectional bridging during the multi-repo
  // encoder soft transition so V1 and V2 callers still compare equal via `operator==` while
  // keeping `domain` populated for encoders in petal repos that have not yet rolled.
  {
    fuchsia_media::FormatDetails v1_video;
    v1_video.format_details_version_ordinal() = 2;
    v1_video.mime_type() = "video/raw";
    fuchsia_media::VideoUncompressedFormat uncompressed;
    uncompressed.image_format().pixel_format().type() = fuchsia_sysmem::PixelFormatType::kNv12;
    uncompressed.image_format().coded_width() = 1280;
    uncompressed.image_format().coded_height() = 720;
    uncompressed.image_format().bytes_per_row() = 1280;
    uncompressed.image_format().display_width() = 1280;
    uncompressed.image_format().display_height() = 720;
    uncompressed.image_format().color_space().type() = fuchsia_sysmem::ColorSpaceType::kRec709;
    v1_video.domain() = fuchsia_media::DomainFormat::WithVideo(
        fuchsia_media::VideoFormat::WithUncompressed(std::move(uncompressed)));

    fuchsia_media::FormatDetails v2_video;
    v2_video.format_details_version_ordinal() = 2;
    v2_video.mime_type() = "video/raw";
    auto& img_v2 = v2_video.image_format().emplace();
    img_v2.pixel_format() = fuchsia_images2::PixelFormat::kNv12;
    img_v2.size() = fuchsia_math::SizeU(1280, 720);
    img_v2.valid_size() = fuchsia_math::SizeU(1280, 720);
    img_v2.bytes_per_row() = 1280;
    img_v2.display_rect() = fuchsia_math::RectU(0, 0, 1280, 720);
    img_v2.color_space() = fuchsia_images2::ColorSpace::kRec709;

    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&v1_video);
    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&v2_video);
    EXPECT_TRUE(v1_video.domain().has_value());
    ASSERT_TRUE(v1_video.image_format().has_value());
    EXPECT_TRUE(v2_video.domain().has_value());
    EXPECT_EQ(v1_video, v2_video);
  }

  {
    fuchsia_media::FormatDetails v1_audio;
    v1_audio.format_details_version_ordinal() = 3;
    v1_audio.mime_type() = "audio/raw";
    fuchsia_media::PcmFormat pcm_v1;
    pcm_v1.pcm_mode() = fuchsia_media::AudioPcmMode::kLinear;
    pcm_v1.bits_per_sample() = 16;
    pcm_v1.frames_per_second() = 48000;
    pcm_v1.channel_map() = {
        fuchsia_media::AudioChannelId::kLf,
        fuchsia_media::AudioChannelId::kRf,
    };
    v1_audio.domain() =
        fuchsia_media::DomainFormat::WithAudio(fuchsia_media::AudioFormat::WithUncompressed(
            fuchsia_media::AudioUncompressedFormat::WithPcm(std::move(pcm_v1))));

    fuchsia_media::FormatDetails v2_audio;
    v2_audio.format_details_version_ordinal() = 3;
    v2_audio.mime_type() = "audio/raw";
    auto& pcm_v2 = v2_audio.pcm_format().emplace();
    pcm_v2.pcm_mode() = fuchsia_media::AudioPcmMode2::kLinear;
    pcm_v2.bits_per_sample() = 16;
    pcm_v2.frames_per_second() = 48000;
    pcm_v2.channel_map() = {
        fuchsia_media::AudioChannelId2::kFl,
        fuchsia_media::AudioChannelId2::kFr,
    };

    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&v1_audio);
    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&v2_audio);
    EXPECT_TRUE(v1_audio.domain().has_value());
    ASSERT_TRUE(v1_audio.pcm_format().has_value());
    EXPECT_TRUE(v2_audio.domain().has_value());
    EXPECT_EQ(v1_audio, v2_audio);
  }

  // Decryptor input with `mime_type` set (and no `encrypted_format` yet, e.g. on CreateDecryptor)
  // must NOT synthesize `domain.video.compressed`.
  {
    fuchsia_media::FormatDetails decryptor_init;
    decryptor_init.format_details_version_ordinal() = 0;
    decryptor_init.mime_type() = "video/h264";

    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&decryptor_init, /*is_decryptor=*/true);
    EXPECT_FALSE(decryptor_init.domain().has_value());
    EXPECT_FALSE(decryptor_init.encrypted_format().has_value());
  }

  // Secure decoder input (`is_decryptor == false`) with `mime_type` + `encrypted_format` (or legacy
  // `domain.crypto.encrypted`) synthesizes `domain.video.compressed` while keeping
  // `encrypted_format` populated without cloning into `domain.crypto`.
  {
    fuchsia_media::FormatDetails secure_decoder_v2;
    secure_decoder_v2.format_details_version_ordinal() = 4;
    secure_decoder_v2.mime_type() = "video/h264";
    auto& enc_v2 = secure_decoder_v2.encrypted_format().emplace();
    enc_v2.scheme() = "cenc";
    enc_v2.key_id() = {1, 2, 3, 4};

    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&secure_decoder_v2,
                                                            /*is_decryptor=*/false);
    ASSERT_TRUE(secure_decoder_v2.encrypted_format().has_value());
    ASSERT_TRUE(secure_decoder_v2.domain().has_value());
    ASSERT_EQ(secure_decoder_v2.domain()->Which(), fuchsia_media::DomainFormat::Tag::kVideo);
    EXPECT_EQ(secure_decoder_v2.domain()->video()->Which(),
              fuchsia_media::VideoFormat::Tag::kCompressed);

    fuchsia_media::FormatDetails secure_decoder_v1;
    secure_decoder_v1.format_details_version_ordinal() = 4;
    secure_decoder_v1.mime_type() = "video/h264";
    fuchsia_media::EncryptedFormat enc_v1;
    enc_v1.scheme() = "cenc";
    enc_v1.key_id() = {1, 2, 3, 4};
    secure_decoder_v1.domain() = fuchsia_media::DomainFormat::WithCrypto(
        fuchsia_media::CryptoFormat::WithEncrypted(std::move(enc_v1)));

    CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&secure_decoder_v1,
                                                            /*is_decryptor=*/false);
    EXPECT_EQ(secure_decoder_v1, secure_decoder_v2);
  }
}
