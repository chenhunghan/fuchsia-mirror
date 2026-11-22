// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/lib/escher/renderer/batch_gpu_uploader.h"

#include <cstring>
#include <thread>

#include <gtest/gtest.h>

#include "src/ui/lib/escher/geometry/types.h"
#include "src/ui/lib/escher/impl/vulkan_utils.h"
#include "src/ui/lib/escher/test/common/gtest_escher.h"
#include "src/ui/lib/escher/util/image_utils.h"
#include "src/ui/lib/escher/vk/buffer_factory.h"
#include "src/ui/lib/escher/vk/command_buffer.h"
#include "src/ui/lib/escher/vk/image_factory.h"

namespace escher {

namespace {

std::pair<ImagePtr, vk::BufferImageCopy> Create1x1ImageAndRegion(EscherWeakPtr escher) {
  // Create a 1x1 RGBA (8-bit channels) image to write to.
  ImageFactoryAdapter image_factory(escher->gpu_allocator(), escher->resource_recycler());
  ImagePtr image =
      image_utils::NewImage(&image_factory, vk::Format::eR8G8B8A8Unorm, ColorSpace::kSrgb, 1, 1);
  vk::BufferImageCopy region;
  region.imageSubresource.aspectMask = vk::ImageAspectFlagBits::eColor;
  region.imageSubresource.mipLevel = 0;
  region.imageSubresource.baseArrayLayer = 0;
  region.imageSubresource.layerCount = 1;
  region.imageExtent.width = 1;
  region.imageExtent.height = 1;
  region.imageExtent.depth = 1;
  region.bufferOffset = 0;
  return {std::move(image), std::move(region)};
}

// Copies the color contents of `image`, which holds `byte_size` tightly packed bytes, to host
// memory and returns them. Waits for the GPU to become idle.
std::vector<uint8_t> ReadbackColorImage(const EscherWeakPtr& escher, const ImagePtr& image,
                                        vk::DeviceSize byte_size) {
  BufferPtr readback_buffer = escher->buffer_cache()->NewHostBuffer(byte_size);
  auto cmds = CommandBuffer::NewForTransfer(escher.get());
  constexpr auto kStage = vk::PipelineStageFlagBits::eTransfer;
  cmds->ImageBarrier(image, image->layout(), vk::ImageLayout::eTransferSrcOptimal, kStage,
                     vk::AccessFlagBits::eTransferRead | vk::AccessFlagBits::eTransferWrite, kStage,
                     vk::AccessFlagBits::eTransferRead);
  const vk::BufferImageCopy region =
      impl::GetDefaultBufferImageCopy(image->width(), image->height());
  cmds->vk().copyImageToBuffer(image->vk(), vk::ImageLayout::eTransferSrcOptimal,
                               readback_buffer->vk(), 1, &region);
  cmds->KeepAlive(readback_buffer);
  cmds->KeepAlive(image);
  EXPECT_TRUE(cmds->Submit(nullptr));
  (void)escher->vk_device().waitIdle();
  EXPECT_TRUE(escher->Cleanup());

  const uint8_t* data = readback_buffer->host_ptr();
  return std::vector<uint8_t>(data, data + byte_size);
}

// Wraps the `vk::Image` of `owner` without taking ownership. Like other images created by
// `Image::WrapVkImage()`, the returned image reports a `size()` of 0. `owner` must outlive it.
ImagePtr WrapImage(const EscherWeakPtr& escher, const ImagePtr& owner) {
  return Image::WrapVkImage(escher->resource_recycler(), owner->info(), owner->vk(),
                            owner->layout());
}

// Submits all work scheduled on `uploader`, waits for it to finish, and returns the result of
// `Escher::Cleanup()`. Death tests call this after scheduling so that, if the check under test did
// not fire, the death block would exit cleanly instead of aborting in `~BatchGpuUploader()` due to
// unsubmitted work.
bool SubmitAndWait(const EscherWeakPtr& escher, BatchGpuUploader& uploader) {
  uploader.Submit();
  (void)escher->vk_device().waitIdle();
  return escher->Cleanup();
}

// Expects that scheduling a 4-byte upload of `region` to `image` kills the process with a message
// matching `message`.
void ExpectScheduleWriteImageDies(const EscherWeakPtr& escher, const ImagePtr& image,
                                  const vk::BufferImageCopy& region, const char* message) {
  EXPECT_DEATH(
      {
        auto uploader = BatchGpuUploader::New(escher);
        uploader->ScheduleWriteImage(image, std::vector<uint8_t>{1, 2, 3, 4},
                                     vk::ImageLayout::eShaderReadOnlyOptimal, region);
        SubmitAndWait(escher, *uploader);
      },
      message);
}

}  // namespace

using BatchGpuUploaderTest = test::TestWithVkValidationLayer;

VK_TEST_F(BatchGpuUploaderTest, CreateDestroyUploader) {
  auto escher = test::GetEscher()->GetWeakPtr();
  bool batch_upload_done = false;

  {
    std::unique_ptr<BatchGpuUploader> uploader = BatchGpuUploader::New(escher);
    // BatchGpuUploader must be submitted before it is destroyed.
    uploader->Submit([&batch_upload_done]() { batch_upload_done = true; });
  }

  (void)escher->vk_device().waitIdle();
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_TRUE(batch_upload_done);
}

VK_TEST_F(BatchGpuUploaderTest, InvalidUploader) {
  // A BatchGpuUploader without an escher should not be created.
  auto uploader = BatchGpuUploader::New(EscherWeakPtr());
  EXPECT_FALSE(uploader);
}

VK_TEST_F(BatchGpuUploaderTest, CallbackTriggeredOnEmptyUploader) {
  auto escher = test::GetEscher()->GetWeakPtr();
  std::unique_ptr<BatchGpuUploader> uploader = BatchGpuUploader::New(escher);

  EXPECT_FALSE(uploader->HasContentToUpload());
  bool callback_executed = false;

  uploader->Submit([&callback_executed] { callback_executed = true; });
  (void)escher->vk_device().waitIdle();
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_TRUE(callback_executed);
}

VK_TEST_F(BatchGpuUploaderTest, WriteBufferUsingWriteFunction) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto uploader = BatchGpuUploader::New(escher);
  const size_t buffer_size = 3 * sizeof(vec3);
  // Create buffer to write to.
  BufferFactoryAdapter buffer_factory(escher->gpu_allocator(), escher->resource_recycler());
  BufferPtr vertex_buffer = buffer_factory.NewBuffer(
      buffer_size, vk::BufferUsageFlagBits::eVertexBuffer | vk::BufferUsageFlagBits::eTransferDst,
      vk::MemoryPropertyFlagBits::eDeviceLocal);

  // Do write.
  bool write_finished = false;
  uploader->ScheduleWriteBuffer(
      vertex_buffer,
      [&write_finished](uint8_t* host_ptr, size_t size) {
        EXPECT_TRUE(size >= sizeof(vec3) * 3);
        vec3* verts = reinterpret_cast<vec3*>(host_ptr);
        verts[0] = vec3(0.f, 0.f, 0.f);
        verts[1] = vec3(0.f, 1.f, 0.f);
        verts[2] = vec3(1.f, 0.f, 0.f);
        write_finished = true;
      },
      /* target_offset */ 0, /* copy_size */ buffer_size);
  // The write is deferred until we generate the commands.
  EXPECT_FALSE(write_finished);

  // Submit the work.
  bool batch_upload_done = false;
  uploader->Submit([&batch_upload_done]() { batch_upload_done = true; });
  EXPECT_TRUE(write_finished);

  (void)escher->vk_device().waitIdle();
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_TRUE(batch_upload_done);
}

VK_TEST_F(BatchGpuUploaderTest, WriteBufferUsingVectorOfUint8) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto uploader = BatchGpuUploader::New(escher);
  const size_t buffer_size = 3 * sizeof(vec3);
  // Create buffer to write to.
  BufferFactoryAdapter buffer_factory(escher->gpu_allocator(), escher->resource_recycler());
  BufferPtr vertex_buffer = buffer_factory.NewBuffer(
      buffer_size, vk::BufferUsageFlagBits::eVertexBuffer | vk::BufferUsageFlagBits::eTransferDst,
      vk::MemoryPropertyFlagBits::eDeviceLocal);

  // Do write.
  std::vector<uint8_t> host_data(buffer_size);
  vec3* verts = reinterpret_cast<vec3*>(host_data.data());
  verts[0] = vec3(0.f, 0.f, 0.f);
  verts[1] = vec3(0.f, 1.f, 0.f);
  verts[2] = vec3(1.f, 0.f, 0.f);
  uploader->ScheduleWriteBuffer(vertex_buffer, std::move(host_data));

  // Submit the work.
  bool batch_upload_done = false;
  uploader->Submit([&batch_upload_done]() { batch_upload_done = true; });
  (void)escher->vk_device().waitIdle();
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_TRUE(batch_upload_done);
}

VK_TEST_F(BatchGpuUploaderTest, WriteBufferUsingVectorOfAnyType) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto uploader = BatchGpuUploader::New(escher);
  const size_t buffer_size = 3 * sizeof(vec3);
  // Create buffer to write to.
  BufferFactoryAdapter buffer_factory(escher->gpu_allocator(), escher->resource_recycler());
  BufferPtr vertex_buffer = buffer_factory.NewBuffer(
      buffer_size, vk::BufferUsageFlagBits::eVertexBuffer | vk::BufferUsageFlagBits::eTransferDst,
      vk::MemoryPropertyFlagBits::eDeviceLocal);

  // Do write.
  std::vector<vec3> verts(3);
  verts[0] = vec3(0.f, 0.f, 0.f);
  verts[1] = vec3(0.f, 1.f, 0.f);
  verts[2] = vec3(1.f, 0.f, 0.f);

  uploader->ScheduleWriteBuffer(vertex_buffer, verts);

  // Submit the work.
  bool batch_upload_done = false;
  uploader->Submit([&batch_upload_done]() { batch_upload_done = true; });
  (void)escher->vk_device().waitIdle();
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_TRUE(batch_upload_done);
}

VK_TEST_F(BatchGpuUploaderTest, LazyInitializationTest) {
  auto escher = test::GetEscher()->GetWeakPtr();
  std::unique_ptr<BatchGpuUploader> uploader = BatchGpuUploader::New(escher);

  constexpr vk::DeviceSize kBufferSize = 1024;

  BufferFactoryAdapter buffer_factory(escher->gpu_allocator(), escher->resource_recycler());
  BufferPtr buffer = buffer_factory.NewBuffer(
      kBufferSize, vk::BufferUsageFlagBits::eVertexBuffer | vk::BufferUsageFlagBits::eTransferDst,
      vk::MemoryPropertyFlagBits::eDeviceLocal);

  // BatchGpuUploader will not be initialized until instantiation of Writers.
  EXPECT_FALSE(uploader->HasContentToUpload());

  std::vector<uint8_t> host_data(kBufferSize, 0x7f);
  uploader->ScheduleWriteBuffer(buffer, std::move(host_data));

  EXPECT_TRUE(uploader->HasContentToUpload());

  // BatchGpuUploader must be submitted before it is destroyed.
  bool batch_upload_done = false;
  uploader->Submit([&batch_upload_done]() { batch_upload_done = true; });
  (void)escher->vk_device().waitIdle();
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_TRUE(batch_upload_done);
}

VK_TEST_F(BatchGpuUploaderTest, WriteImageUsingWriteFunction) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto uploader = BatchGpuUploader::New(escher);
  const size_t image_size = sizeof(uint8_t) * 4;

  // Create a 1x1 RGBA (8-bit channels) image to write to.
  auto [image, region] = Create1x1ImageAndRegion(escher);

  // Do write.
  constexpr vk::ImageLayout kTargetImageLayout = vk::ImageLayout::eShaderReadOnlyOptimal;
  bool write_finished = false;
  uploader->ScheduleWriteImage(
      image,
      [&write_finished](uint8_t* host_ptr, size_t data_size) {
        EXPECT_TRUE(data_size >= 4);
        host_ptr[0] = 150;
        host_ptr[1] = 88;
        host_ptr[2] = 121;
        host_ptr[3] = 255;
        write_finished = true;
      },
      kTargetImageLayout);
  // The write is deferred until we generate the commands.
  EXPECT_FALSE(write_finished);

  // Submit the work.
  bool batch_upload_done = false;
  uploader->Submit([&batch_upload_done]() { batch_upload_done = true; });
  EXPECT_TRUE(write_finished);

  (void)escher->vk_device().waitIdle();
  // Verify that the image layout was set correctly.
  EXPECT_TRUE(image->layout() == kTargetImageLayout);
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_TRUE(batch_upload_done);
}

VK_TEST_F(BatchGpuUploaderTest, WriteImageUsingVectorOfUint8) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto uploader = BatchGpuUploader::New(escher);
  const size_t image_size = sizeof(uint8_t) * 4;

  // Create a 1x1 RGBA (8-bit channels) image to write to.
  auto [image, region] = Create1x1ImageAndRegion(escher);

  // Do write.
  constexpr vk::ImageLayout kTargetImageLayout = vk::ImageLayout::eShaderReadOnlyOptimal;
  std::vector<uint8_t> pixels(image_size);
  pixels[0] = 150;
  pixels[1] = 88;
  pixels[2] = 121;
  pixels[3] = 255;
  uploader->ScheduleWriteImage(image, std::move(pixels), kTargetImageLayout, region);

  // Submit the work.
  bool batch_upload_done = false;
  uploader->Submit([&batch_upload_done]() { batch_upload_done = true; });

  (void)escher->vk_device().waitIdle();
  // Verify that the image layout was set correctly.
  EXPECT_TRUE(image->layout() == kTargetImageLayout);
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_TRUE(batch_upload_done);
}

VK_TEST_F(BatchGpuUploaderTest, WriteImageUsingVectorOfAnyType) {
  struct RGBA {
    uint8_t r, g, b, a;
  };

  auto escher = test::GetEscher()->GetWeakPtr();
  auto uploader = BatchGpuUploader::New(escher);

  // Create a 1x1 RGBA (8-bit channels) image to write to.
  auto [image, region] = Create1x1ImageAndRegion(escher);

  // Do write.
  constexpr vk::ImageLayout kTargetImageLayout = vk::ImageLayout::eShaderReadOnlyOptimal;
  std::vector<RGBA> pixels;
  pixels.push_back({150, 88, 121, 255});
  uploader->ScheduleWriteImage(image, std::move(pixels), kTargetImageLayout, region);

  // Submit the work.
  bool batch_upload_done = false;
  uploader->Submit([&batch_upload_done]() { batch_upload_done = true; });

  (void)escher->vk_device().waitIdle();
  // Verify that the image layout was set correctly.
  EXPECT_TRUE(image->layout() == kTargetImageLayout);
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_TRUE(batch_upload_done);
}

// This unit tests if we can upload to the same image multiple times and if
// the image layout can be set correctly every time we upload the image.
VK_TEST_F(BatchGpuUploaderTest, ChangeLayout) {
  auto escher = test::GetEscher()->GetWeakPtr();

  // Create a 1x1 RGBA (8-bit channels) image to write to.
  std::vector<uint8_t> pixel_1 = {150, 88, 121, 255};
  auto [image, region] = Create1x1ImageAndRegion(escher);

  // Do write.
  auto uploader = BatchGpuUploader::New(escher);
  constexpr vk::ImageLayout kTargetImageLayout = vk::ImageLayout::eGeneral;
  uploader->ScheduleWriteImage(image, std::move(pixel_1), kTargetImageLayout, region);

  // Submit the work.
  bool batch_upload_done = false;
  uploader->Submit([&batch_upload_done]() { batch_upload_done = true; });
  (void)escher->vk_device().waitIdle();
  EXPECT_TRUE(image->layout() == kTargetImageLayout);
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_TRUE(batch_upload_done);

  // Write the image again and change the image layout to another layout.
  auto uploader_2 = BatchGpuUploader::New(escher);
  constexpr vk::ImageLayout kTargetImageLayout_2 = vk::ImageLayout::eShaderReadOnlyOptimal;
  std::vector<uint8_t> pixel_2 = {130, 120, 110, 255};
  uploader_2->ScheduleWriteImage(image, std::move(pixel_2), kTargetImageLayout_2, region);

  // Submit the work.
  bool batch_upload_done_2 = false;
  uploader_2->Submit([&batch_upload_done_2]() { batch_upload_done_2 = true; });
  (void)escher->vk_device().waitIdle();

  // Verify that the image layout was set correctly.
  EXPECT_TRUE(image->layout() == kTargetImageLayout_2);
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_TRUE(batch_upload_done_2);
}

VK_TEST_F(BatchGpuUploaderTest, SubmitImageToCommandBuffer) {
  auto escher = test::GetEscher()->GetWeakPtr();

  // Create a 1x1 RGBA (8-bit channels) image to write to.
  std::vector<uint8_t> pixel_1 = {150, 88, 121, 255};
  auto [image, region] = Create1x1ImageAndRegion(escher);

  // Do write.
  auto uploader = BatchGpuUploader::New(escher);
  constexpr vk::ImageLayout kTargetImageLayout = vk::ImageLayout::eGeneral;
  uploader->ScheduleWriteImage(image, std::move(pixel_1), kTargetImageLayout, region);

  auto cmds = CommandBuffer::NewForTransfer(escher.get());
  uploader->GenerateCommands(cmds.get());
  bool uploaded = false;
  cmds->Submit([&uploaded]() { uploaded = true; });
  EXPECT_FALSE(uploader->HasContentToUpload());

  (void)escher->vk_device().waitIdle();
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_TRUE(uploaded);

  // Check if the uploaded content is correct.
  EXPECT_EQ(ReadbackColorImage(escher, image, 4), (std::vector<uint8_t>{150, 88, 121, 255}));
}

VK_TEST_F(BatchGpuUploaderTest, ReuseAfterSubmission) {
  auto escher = test::GetEscher()->GetWeakPtr();

  // Create a 1x1 RGBA (8-bit channels) image to write to.
  std::vector<uint8_t> pixel_1 = {150, 88, 121, 255};
  std::vector<uint8_t> pixel_2 = {130, 120, 110, 255};
  auto [image_1, region_1] = Create1x1ImageAndRegion(escher);
  auto [image_2, region_2] = Create1x1ImageAndRegion(escher);

  // Do write.
  auto uploader = BatchGpuUploader::New(escher);
  constexpr vk::ImageLayout kTargetImageLayout = vk::ImageLayout::eGeneral;
  uploader->ScheduleWriteImage(image_1, std::move(pixel_1), kTargetImageLayout, region_1);
  EXPECT_TRUE(uploader->HasContentToUpload());

  bool uploaded_1 = false;
  uploader->Submit([&uploaded_1]() { uploaded_1 = true; });
  EXPECT_FALSE(uploader->HasContentToUpload());

  // Schedule another write after submission.
  uploader->ScheduleWriteImage(image_2, std::move(pixel_2), kTargetImageLayout, region_2);
  EXPECT_TRUE(uploader->HasContentToUpload());

  bool uploaded_2 = false;
  uploader->Submit([&uploaded_2]() { uploaded_2 = true; });
  EXPECT_FALSE(uploader->HasContentToUpload());

  (void)escher->vk_device().waitIdle();
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_TRUE(uploaded_1 && uploaded_2);
}

VK_TEST_F(BatchGpuUploaderTest, WriteBufferWithOffsetOutOfBoundsDeathTest) {
  auto escher = test::GetEscher()->GetWeakPtr();
  const size_t buffer_size = 3 * sizeof(vec3);
  // Create buffer to write to.
  BufferFactoryAdapter buffer_factory(escher->gpu_allocator(), escher->resource_recycler());
  BufferPtr vertex_buffer = buffer_factory.NewBuffer(
      buffer_size, vk::BufferUsageFlagBits::eVertexBuffer | vk::BufferUsageFlagBits::eTransferDst,
      vk::MemoryPropertyFlagBits::eDeviceLocal);

  // Do write with an offset out of bounds
  // This explicitly tests the integer underflow edge case.
  EXPECT_DEATH(
      {
        auto uploader = BatchGpuUploader::New(escher);
        uploader->ScheduleWriteBuffer(
            vertex_buffer, [](uint8_t* host_ptr, size_t size) {},
            /* target_offset */ buffer_size + 10, /* copy_size */ buffer_size);
      },
      "");
}

VK_TEST_F(BatchGpuUploaderTest, UnfinishedWorkDeathTest) {
  auto escher = test::GetEscher()->GetWeakPtr();

  auto [image, region] = Create1x1ImageAndRegion(escher);
  std::vector<uint8_t> pixel_1 = {150, 88, 121, 255};
  constexpr vk::ImageLayout kTargetImageLayout = vk::ImageLayout::eGeneral;

  // Submit should fail since we scheduled image write but didn't submit that.
  EXPECT_DEATH(
      {
        auto uploader = BatchGpuUploader::New(escher);
        uploader->ScheduleWriteImage(image, std::move(pixel_1), kTargetImageLayout, region);
        uploader = nullptr;
      },
      "");
}

// The callback receives the byte size of the copy region, even when `Image::size()` is 0, as it is
// for images created by `Image::WrapVkImage()`.
VK_TEST_F(BatchGpuUploaderTest, WriteImageCallbackReceivesRegionByteSize) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto [owner, region] = Create1x1ImageAndRegion(escher);
  ImagePtr wrapped = WrapImage(escher, owner);
  auto uploader = BatchGpuUploader::New(escher);

  size_t received_copy_size = 0;
  uploader->ScheduleWriteImage(
      wrapped,
      [&received_copy_size](uint8_t* host_ptr, size_t copy_size) {
        received_copy_size = copy_size;
        std::memset(host_ptr, 0xFF, copy_size);
      },
      vk::ImageLayout::eShaderReadOnlyOptimal, region);
  EXPECT_TRUE(SubmitAndWait(escher, *uploader));

  EXPECT_EQ(received_copy_size, 4U);
}

// Images created by `Image::WrapVkImage()` report a size of 0; uploading to
// them must still stage and copy every pixel.
VK_TEST_F(BatchGpuUploaderTest, WriteWrappedImage) {
  auto escher = test::GetEscher()->GetWeakPtr();
  ImageFactoryAdapter image_factory(escher->gpu_allocator(), escher->resource_recycler());
  ImagePtr owner =
      image_utils::NewImage(&image_factory, vk::Format::eR8G8B8A8Unorm, ColorSpace::kSrgb, 2, 2);
  ImagePtr wrapped = WrapImage(escher, owner);
  ASSERT_EQ(wrapped->size(), 0U);

  const std::vector<uint8_t> pixels = {1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16};
  auto uploader = BatchGpuUploader::New(escher);
  uploader->ScheduleWriteImage(wrapped, pixels, vk::ImageLayout::eTransferSrcOptimal);
  EXPECT_TRUE(SubmitAndWait(escher, *uploader));

  EXPECT_EQ(ReadbackColorImage(escher, wrapped, pixels.size()), pixels);
}

// Several uploads to wrapped images in one batch must get disjoint staging
// ranges so that no upload overwrites another's pixels.
VK_TEST_F(BatchGpuUploaderTest, BatchedWrappedImagesUseDisjointStagingRanges) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto [owner_1, region_1] = Create1x1ImageAndRegion(escher);
  auto [owner_2, region_2] = Create1x1ImageAndRegion(escher);
  ImagePtr wrapped_1 = WrapImage(escher, owner_1);
  ImagePtr wrapped_2 = WrapImage(escher, owner_2);

  const std::vector<uint8_t> first_pixel = {150, 88, 121, 255};
  const std::vector<uint8_t> second_pixel = {130, 120, 110, 255};
  auto uploader = BatchGpuUploader::New(escher);
  uploader->ScheduleWriteImage(wrapped_1, first_pixel, vk::ImageLayout::eTransferSrcOptimal,
                               region_1);
  uploader->ScheduleWriteImage(wrapped_2, second_pixel, vk::ImageLayout::eTransferSrcOptimal,
                               region_2);
  EXPECT_TRUE(SubmitAndWait(escher, *uploader));

  EXPECT_EQ(ReadbackColorImage(escher, wrapped_1, first_pixel.size()), first_pixel);
  EXPECT_EQ(ReadbackColorImage(escher, wrapped_2, second_pixel.size()), second_pixel);
}

// A wrapped image reports a `size()` of 0. If its staging range were sized from `size()`, it would
// start at the same offset as the owned image's range after it, and receive the owned image's
// pixel.
VK_TEST_F(BatchGpuUploaderTest, WrappedAndOwnedImagesInOneBatchUseDisjointStagingRanges) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto [owner, wrapped_region] = Create1x1ImageAndRegion(escher);
  ImagePtr wrapped = WrapImage(escher, owner);
  auto [owned, owned_region] = Create1x1ImageAndRegion(escher);

  const std::vector<uint8_t> wrapped_pixel = {150, 88, 121, 255};
  const std::vector<uint8_t> owned_pixel = {130, 120, 110, 255};
  auto uploader = BatchGpuUploader::New(escher);
  uploader->ScheduleWriteImage(wrapped, wrapped_pixel, vk::ImageLayout::eTransferSrcOptimal,
                               wrapped_region);
  uploader->ScheduleWriteImage(owned, owned_pixel, vk::ImageLayout::eTransferSrcOptimal,
                               owned_region);
  EXPECT_TRUE(SubmitAndWait(escher, *uploader));

  EXPECT_EQ(ReadbackColorImage(escher, wrapped, wrapped_pixel.size()), wrapped_pixel);
  EXPECT_EQ(ReadbackColorImage(escher, owned, owned_pixel.size()), owned_pixel);
}

// A 3x1 `eR8Unorm` upload stages 3 bytes, so the record after it must start at the next 4-byte
// aligned offset. An unaligned `bufferOffset` would be rejected by Vulkan for the 4-byte texels of
// the second image, and an undersized staging buffer would cut off its pixels.
VK_TEST_F(BatchGpuUploaderTest, BatchedUploadAfterUnalignedRecordStartsAligned) {
  auto escher = test::GetEscher()->GetWeakPtr();
  ImageFactoryAdapter image_factory(escher->gpu_allocator(), escher->resource_recycler());
  ImagePtr r8_image =
      image_utils::NewImage(&image_factory, vk::Format::eR8Unorm, ColorSpace::kSrgb, 3, 1);
  auto [rgba_image, rgba_region] = Create1x1ImageAndRegion(escher);

  const std::vector<uint8_t> r8_pixels = {10, 20, 30};
  const std::vector<uint8_t> rgba_pixel = {150, 88, 121, 255};
  auto uploader = BatchGpuUploader::New(escher);
  uploader->ScheduleWriteImage(r8_image, r8_pixels, vk::ImageLayout::eTransferSrcOptimal);
  uploader->ScheduleWriteImage(rgba_image, rgba_pixel, vk::ImageLayout::eTransferSrcOptimal,
                               rgba_region);
  EXPECT_TRUE(SubmitAndWait(escher, *uploader));

  EXPECT_EQ(ReadbackColorImage(escher, r8_image, r8_pixels.size()), r8_pixels);
  EXPECT_EQ(ReadbackColorImage(escher, rgba_image, rgba_pixel.size()), rgba_pixel);
}

// `host_data` may hold more bytes than the copy region reads; only the leading bytes are
// uploaded. The second upload checks that the next record's staging range does not overlap the
// oversized record's region. Bytes the oversized record might copy past its region cannot be
// observed through the public API, because the next record's own write replaces them.
VK_TEST_F(BatchGpuUploaderTest, OversizedImageHostDataUploadsLeadingBytes) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto [first_image, first_region] = Create1x1ImageAndRegion(escher);
  auto [second_image, second_region] = Create1x1ImageAndRegion(escher);

  const std::vector<uint8_t> oversized_host_data = {150, 88, 121, 255, 1, 2, 3, 4};
  const std::vector<uint8_t> first_pixel(oversized_host_data.begin(),
                                         oversized_host_data.begin() + 4);
  const std::vector<uint8_t> second_pixel = {130, 120, 110, 254};
  auto uploader = BatchGpuUploader::New(escher);
  uploader->ScheduleWriteImage(first_image, oversized_host_data,
                               vk::ImageLayout::eTransferSrcOptimal, first_region);
  uploader->ScheduleWriteImage(second_image, second_pixel, vk::ImageLayout::eTransferSrcOptimal,
                               second_region);
  EXPECT_TRUE(SubmitAndWait(escher, *uploader));

  EXPECT_EQ(ReadbackColorImage(escher, first_image, first_pixel.size()), first_pixel);
  EXPECT_EQ(ReadbackColorImage(escher, second_image, second_pixel.size()), second_pixel);
}

// Every format the uploader accepts stages `width * height * bytes-per-pixel` bytes and uploads
// them unchanged.
VK_TEST_F(BatchGpuUploaderTest, UploadsEverySupportedFormat) {
  struct FormatCase {
    vk::Format format;
    size_t expected_copy_size;
  };
  constexpr FormatCase kFormatCases[] = {
      {.format = vk::Format::eR8G8B8A8Unorm, .expected_copy_size = 16},
      {.format = vk::Format::eR8G8B8A8Srgb, .expected_copy_size = 16},
      {.format = vk::Format::eB8G8R8A8Unorm, .expected_copy_size = 16},
      {.format = vk::Format::eB8G8R8A8Srgb, .expected_copy_size = 16},
      {.format = vk::Format::eR8Unorm, .expected_copy_size = 4},
  };

  auto escher = test::GetEscher()->GetWeakPtr();
  ImageFactoryAdapter image_factory(escher->gpu_allocator(), escher->resource_recycler());
  for (const FormatCase& format_case : kFormatCases) {
    SCOPED_TRACE(vk::to_string(format_case.format));
    ImagePtr image =
        image_utils::NewImage(&image_factory, format_case.format, ColorSpace::kSrgb, 2, 2);

    size_t received_copy_size = 0;
    std::vector<uint8_t> written;
    auto uploader = BatchGpuUploader::New(escher);
    uploader->ScheduleWriteImage(
        image,
        [&received_copy_size, &written](uint8_t* host_ptr, size_t copy_size) {
          received_copy_size = copy_size;
          written.resize(copy_size);
          for (size_t i = 0; i < copy_size; ++i) {
            written[i] = static_cast<uint8_t>(i + 1);
          }
          std::memcpy(host_ptr, written.data(), copy_size);
        },
        vk::ImageLayout::eTransferSrcOptimal);
    EXPECT_TRUE(SubmitAndWait(escher, *uploader));

    EXPECT_EQ(received_copy_size, format_case.expected_copy_size);
    EXPECT_EQ(ReadbackColorImage(escher, image, format_case.expected_copy_size), written);
  }
}

// Uploading fewer bytes than the copy region reads would upload stale staging
// memory to the GPU, so it is rejected when the upload is scheduled.
VK_TEST_F(BatchGpuUploaderTest, UndersizedImageHostDataDeathTest) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto [image, region] = Create1x1ImageAndRegion(escher);

  // Match the check's message: in AddressSanitizer builds the 4-byte copy from the 3-byte vector
  // at `Submit()` would also kill the process if the check were missing.
  EXPECT_DEATH(
      {
        auto uploader = BatchGpuUploader::New(escher);
        uploader->ScheduleWriteImage(image, std::vector<uint8_t>{1, 2, 3},
                                     vk::ImageLayout::eShaderReadOnlyOptimal, region);
        SubmitAndWait(escher, *uploader);
      },
      "copy region reads");
}

// Staging ranges are sized for tightly packed regions only, so a region with a
// row length is rejected when the upload is scheduled.
VK_TEST_F(BatchGpuUploaderTest, PitchedRowLengthCopyRegionDeathTest) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto [image, region] = Create1x1ImageAndRegion(escher);
  region.bufferRowLength = 2;

  ExpectScheduleWriteImageDies(escher, image, region, "supported for upload: bufferRowLength is 2");
}

// Staging ranges are sized for color data only, so a depth region is rejected when the upload is
// scheduled.
VK_TEST_F(BatchGpuUploaderTest, UnsupportedAspectCopyRegionDeathTest) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto [image, region] = Create1x1ImageAndRegion(escher);
  region.imageSubresource.aspectMask = vk::ImageAspectFlagBits::eDepth;

  ExpectScheduleWriteImageDies(escher, image, region, "Only color copy regions");
}

// Staging ranges are sized for one layer only, so a multi-layer region is rejected when the
// upload is scheduled.
VK_TEST_F(BatchGpuUploaderTest, MultiLayerCopyRegionDeathTest) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto [image, region] = Create1x1ImageAndRegion(escher);
  region.imageSubresource.layerCount = 2;

  ExpectScheduleWriteImageDies(escher, image, region, "Only single-layer");
}

// Staging ranges are sized for tightly packed regions only, so a region with an image height is
// rejected when the upload is scheduled.
VK_TEST_F(BatchGpuUploaderTest, PitchedImageHeightCopyRegionDeathTest) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto [image, region] = Create1x1ImageAndRegion(escher);
  region.bufferImageHeight = 2;

  ExpectScheduleWriteImageDies(escher, image, region,
                               "supported for upload: bufferImageHeight is 2");
}

// Formats `IsUploadFormatSupported()` rejects crash when the upload is scheduled. 4:2:2 YUV is used
// because `image_utils::GetImageByteSize()` sizes it, so only the `IsUploadFormatSupported()` check
// can fire. No device support for the format is needed: wrapping makes no Vulkan call, and the
// check fires before anything reads the image memory.
VK_TEST_F(BatchGpuUploaderTest, UnsupportedUploadFormatDeathTest) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto [owner, region] = Create1x1ImageAndRegion(escher);
  ImageInfo yuv_info = owner->info();
  yuv_info.format = vk::Format::eG8B8G8R8422Unorm;
  ImagePtr yuv_image =
      Image::WrapVkImage(escher->resource_recycler(), yuv_info, owner->vk(), owner->layout());

  ExpectScheduleWriteImageDies(escher, yuv_image, region, "Unsupported image format for upload");
}

// An empty extent would size an empty staging range, so a zero-width region is rejected when the
// upload is scheduled.
VK_TEST_F(BatchGpuUploaderTest, ZeroWidthCopyRegionDeathTest) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto [image, region] = Create1x1ImageAndRegion(escher);
  region.imageExtent.width = 0;

  ExpectScheduleWriteImageDies(escher, image, region, "imageExtent.width is 0");
}

// An empty extent would size an empty staging range, so a zero-height region is rejected when the
// upload is scheduled.
VK_TEST_F(BatchGpuUploaderTest, ZeroHeightCopyRegionDeathTest) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto [image, region] = Create1x1ImageAndRegion(escher);
  region.imageExtent.height = 0;

  ExpectScheduleWriteImageDies(escher, image, region, "imageExtent.height is 0");
}

// Staging ranges are sized for 2D regions only, so a zero-depth region is rejected when the upload
// is scheduled.
VK_TEST_F(BatchGpuUploaderTest, ZeroDepthCopyRegionDeathTest) {
  auto escher = test::GetEscher()->GetWeakPtr();
  auto [image, region] = Create1x1ImageAndRegion(escher);
  region.imageExtent.depth = 0;

  ExpectScheduleWriteImageDies(escher, image, region, "imageExtent.depth is 0");
}

}  // namespace escher
