// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/lib/escher/impl/naive_image.h"

#include <gtest/gtest.h>

#include "src/ui/lib/escher/escher.h"
#include "src/ui/lib/escher/impl/vulkan_utils.h"
#include "src/ui/lib/escher/resources/resource_recycler.h"
#include "src/ui/lib/escher/test/common/gtest_escher.h"
#include "src/ui/lib/escher/util/image_utils.h"
#include "src/ui/lib/escher/vk/gpu_mem.h"
#include "src/ui/lib/escher/vk/texture.h"

namespace escher {
namespace {

using NaiveImageBoundAdoptionTest = test::TestWithVkValidationLayer;

// Verifies that an already-bound `vk::Image` can be adopted via `AdoptBoundVkImage()` and wrapped
// in a `Texture` without calling `vkBindImageMemory()` a second time (which Vulkan validation
// rejects under VUID-vkBindImageMemory-image-07460). Catching a duplicate bind via
// `EXPECT_VULKAN_VALIDATION_OK()` relies on the Vulkan validation layer bundled with the Escher
// test package.
VK_TEST_F(NaiveImageBoundAdoptionTest, AdoptBoundVkImageWrapsAndDestroysCleanly) {
  auto escher = test::GetEscher();
  auto device = escher->vk_device();
  auto physical_device = escher->vk_physical_device();

  const ImageInfo info = {
      .format = vk::Format::eR8G8B8A8Unorm,
      .width = 64,
      .height = 32,
      .sample_count = 1,
      .usage = vk::ImageUsageFlagBits::eSampled,
      .memory_flags = vk::MemoryPropertyFlagBits::eDeviceLocal,
      .color_space = ColorSpace::kSrgb,
  };

  vk::Image vk_image = image_utils::CreateVkImage(device, info, vk::ImageLayout::ePreinitialized);
  ASSERT_TRUE(vk_image);

  const vk::MemoryRequirements mem_requirements = device.getImageMemoryRequirements(vk_image);
  vk::MemoryAllocateInfo alloc_info;
  alloc_info.allocationSize = mem_requirements.alignment + mem_requirements.size;
  alloc_info.memoryTypeIndex = impl::GetMemoryTypeIndex(
      physical_device, mem_requirements.memoryTypeBits, vk::MemoryPropertyFlagBits::eDeviceLocal);
  vk::DeviceMemory vk_mem = ESCHER_CHECKED_VK_RESULT(device.allocateMemory(alloc_info));

  GpuMemPtr slab = GpuMem::AdoptVkMemory(device, vk_mem, alloc_info.allocationSize,
                                         /*needs_mapped_ptr=*/false);
  ASSERT_TRUE(slab);
  GpuMemPtr mem = slab->Suballocate(mem_requirements.size, mem_requirements.alignment);
  ASSERT_TRUE(mem);
  const GpuMem* expected_mem = mem.get();
  ASSERT_EQ(device.bindImageMemory(vk_image, mem->base(), mem->offset()), vk::Result::eSuccess);

  ImagePtr image =
      impl::NaiveImage::AdoptBoundVkImage(escher->resource_recycler(), info, vk_image,
                                          std::move(mem), vk::ImageLayout::ePreinitialized);
  ASSERT_TRUE(image);
  EXPECT_EQ(image->vk(), vk_image);
  EXPECT_EQ(image->info(), info);
  EXPECT_EQ(image->size(), mem_requirements.size);
  EXPECT_EQ(image->layout(), vk::ImageLayout::ePreinitialized);
  EXPECT_EQ(static_cast<impl::NaiveImage*>(image.get())->memory().get(), expected_mem);

  TexturePtr texture =
      Texture::New(escher->resource_recycler(), std::move(image), vk::Filter::eLinear);
  ASSERT_TRUE(texture);
  EXPECT_FALSE(slab->HasOneRef());

  texture.reset();
  EXPECT_TRUE(slab->HasOneRef());
  slab.reset();
  EXPECT_VULKAN_VALIDATION_OK();
}

}  // namespace
}  // namespace escher
