// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/lib/escher/vk/impl/descriptor_set_allocator_cache.h"

#include <lib/fit/defer.h>

#include "gtest/gtest.h"
#include "src/ui/lib/escher/impl/command_buffer.h"
#include "src/ui/lib/escher/impl/command_buffer_pool.h"
#include "src/ui/lib/escher/impl/command_buffer_sequencer.h"
#include "src/ui/lib/escher/impl/vulkan_utils.h"
#include "src/ui/lib/escher/test/common/gtest_escher.h"
#include "src/ui/lib/escher/vk/color_space.h"

namespace {
using namespace escher;
using namespace escher::impl;

namespace {

// Select a proper format and filter pair supported by physical device.
std::pair<vk::Format, vk::Filter> SelectSupportedFormatAndFilter(Escher* escher) {
  // We use eNearest as filter since it doesn't depend on any format
  // features specific to physical devices.
  const auto kFilter = vk::Filter::eNearest;

  // For formats, we prefer YUV formats if any of them are supported; otherwise
  // we just choose a normal RGB format.
  const vk::Format kYuvFormats[] = {vk::Format::eG8B8G8R8422Unorm,
                                    vk::Format::eG8B8R82Plane420Unorm,
                                    vk::Format::eG8B8R83Plane420Unorm};
  for (const auto yuv_format : kYuvFormats) {
    if (impl::IsYuvConversionSupported(escher->vk_physical_device(), yuv_format)) {
      return {yuv_format, kFilter};
    }
  }
  return {vk::Format::eR8G8B8Srgb, kFilter};
}
}  // namespace

VK_TEST(DescriptorSetAllocatorCache, LazyCaching) {
  auto escher = escher::test::GetEscher();
  DescriptorSetAllocatorCache cache(escher->vk_device(), escher->GetWeakPtr());

  DescriptorSetLayout layout1 = {};
  layout1.sampled_image_mask = 0x5;
  layout1.storage_image_mask = 0x2;
  layout1.input_attachment_mask = 0x8;
  auto a1 = cache.ObtainDescriptorSetAllocator(layout1, nullptr);
  auto a2 = cache.ObtainDescriptorSetAllocator(layout1, nullptr);
  EXPECT_EQ(a1, a2);
  EXPECT_EQ(1U, cache.size());

  DescriptorSetLayout layout2 = {};
  layout1.sampled_image_mask = 0x5;
  layout1.storage_image_mask = 0x2;
  layout1.input_attachment_mask = 0x10;
  auto a3 = cache.ObtainDescriptorSetAllocator(layout2, nullptr);
  EXPECT_NE(a1, a3);
  EXPECT_EQ(2U, cache.size());

  auto [format, filter] = SelectSupportedFormatAndFilter(escher);
  auto sampler = fxl::MakeRefCounted<Sampler>(escher->resource_recycler(), format, filter,
                                              GetDefaultColorSpace(format), true);
  auto a4 = cache.ObtainDescriptorSetAllocator(layout1, sampler);
  auto a5 = cache.ObtainDescriptorSetAllocator(layout1, sampler);
  EXPECT_NE(a1, a4);
  EXPECT_EQ(a4, a5);
  EXPECT_EQ(3U, cache.size());
}

VK_TEST(DescriptorSetAllocatorCache, ClearsReleasedDescriptorSetAllocator) {
  auto escher = escher::test::GetEscher();
  DescriptorSetAllocatorCache cache(escher->vk_device(), escher->GetWeakPtr());

  DescriptorSetLayout layout1 = {};
  layout1.sampled_image_mask = 0x5;
  layout1.storage_image_mask = 0x2;
  layout1.input_attachment_mask = 0x8;
  auto a1 = cache.ObtainDescriptorSetAllocator(layout1, nullptr);
  EXPECT_EQ(1U, cache.size());

  a1.reset();
  cache.BeginFrame(/*last_finished_sequence_number=*/0);
  EXPECT_EQ(0U, cache.size());
}

VK_TEST(DescriptorSetAllocatorCache, ForwardsSequenceNumberAndEscher) {
  Escher* escher = escher::test::GetEscher();
  EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
  EXPECT_TRUE(escher->Cleanup());
  DescriptorSetAllocatorCache cache(escher->vk_device(), escher->GetWeakPtr());

  // `BeginFrame()` forwards `last_finished_sequence_number` to each allocator:
  // a set referenced by sequence 10 stays cached past the eviction window while
  // `last_finished_sequence_number` is 9, and is evicted once it reaches 10.
  DescriptorSetLayout sequence_layout = {};
  sequence_layout.sampled_image_mask = 0x5;
  sequence_layout.storage_image_mask = 0x2;
  sequence_layout.input_attachment_mask = 0x8;
  auto sequence_allocator = cache.ObtainDescriptorSetAllocator(sequence_layout, nullptr);
  EXPECT_FALSE(sequence_allocator->Get({1}, /*sequence_number=*/10).second);
  for (int i = 0; i < 5; ++i) {
    cache.BeginFrame(/*last_finished_sequence_number=*/9);
  }
  EXPECT_TRUE(sequence_allocator->Get({1}, /*sequence_number=*/10).second);
  for (int i = 0; i < 5; ++i) {
    cache.BeginFrame(/*last_finished_sequence_number=*/9);
  }
  cache.BeginFrame(/*last_finished_sequence_number=*/10);
  EXPECT_FALSE(sequence_allocator->Get({1}, /*sequence_number=*/11).second);

  // `ObtainDescriptorSetAllocator()` forwards `escher_`, so exhausting block 0
  // while every set is unretired but already finished on the GPU reclaims them
  // through `Escher` instead of allocating a second block.
  impl::CommandBuffer* cb = escher->command_buffer_pool()->GetCommandBuffer();
  bool cb_submitted = false;
  auto submit_cb = fit::defer([&] {
    if (!cb_submitted) {
      EXPECT_TRUE(cb->Submit(escher->device()->vk_main_queue(), nullptr));
    }
    EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
    EXPECT_TRUE(escher->Cleanup());
  });
  const uint64_t seq = cb->sequence_number();
  const uint64_t pre_submit_finished =
      escher->command_buffer_sequencer()->last_finished_sequence_number();

  DescriptorSetLayout escher_layout = {};
  escher_layout.sampled_image_mask = 0x1;
  auto escher_allocator = cache.ObtainDescriptorSetAllocator(escher_layout, nullptr);
  cache.BeginFrame(pre_submit_finished);
  constexpr size_t kInitialBlockSize = 64;
  for (size_t i = 0; i < kInitialBlockSize; ++i) {
    EXPECT_FALSE(escher_allocator->Get({i + 1}, seq).second);
  }
  EXPECT_EQ(1U, escher_allocator->num_pool_blocks());

  cb_submitted = true;
  ASSERT_TRUE(cb->Submit(escher->device()->vk_main_queue(), nullptr));
  ASSERT_VK_SUCCESS(escher->vk_device().waitIdle());
  for (int i = 0; i < 5; ++i) {
    cache.BeginFrame(pre_submit_finished);
  }

  EXPECT_FALSE(escher_allocator->Get({kInitialBlockSize + 1}, seq + 1).second);
  EXPECT_EQ(1U, escher_allocator->num_pool_blocks());
}

}  // namespace
