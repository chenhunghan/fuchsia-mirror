// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <memory>

#include <gtest/gtest.h>

#include "src/ui/lib/escher/escher.h"
#include "src/ui/lib/escher/test/common/gtest_escher.h"
#include "src/ui/lib/escher/vk/image.h"
#include "src/ui/lib/escher/vk/vma_gpu_allocator.h"

// This test intentionally aborts, so it lives in `escher_fatal_unittests`, whose package allows
// FATAL logs and whose component manifest uses `fuchsia.process.Launcher` to spawn death-test child
// processes.

namespace {

using escher::VmaGpuAllocator;

// Keeps device-local and host-visible allocations alive together so that, on devices whose memory
// types map to different heaps, the reported count only matches if `~VmaGpuAllocator()` sums the
// live allocations across all heaps.
VK_TEST(VmaGpuAllocatorDeathTest, DestroyingAllocatorWithLiveAllocationsAborts) {
  escher::ImageInfo image_info;
  image_info.format = vk::Format::eR8G8B8A8Unorm;
  image_info.width = 1;
  image_info.height = 1;
  image_info.usage = vk::ImageUsageFlagBits::eTransferDst;
  image_info.memory_flags = vk::MemoryPropertyFlagBits::eDeviceLocal;
  image_info.color_space = escher::ColorSpace::kSrgb;

  EXPECT_DEATH(
      {
        auto allocator =
            std::make_unique<VmaGpuAllocator>(escher::test::GetEscher()->vulkan_context());
        auto mem = allocator->AllocateMemory({1024, 0, 0xffffffff},
                                             vk::MemoryPropertyFlagBits::eDeviceLocal);
        auto buffer = allocator->AllocateBuffer(
            /*manager=*/nullptr, 1024,
            vk::BufferUsageFlagBits::eTransferSrc | vk::BufferUsageFlagBits::eTransferDst,
            vk::MemoryPropertyFlagBits::eHostVisible | vk::MemoryPropertyFlagBits::eHostCoherent,
            /*out_ptr=*/nullptr);
        auto image = allocator->AllocateImage(/*manager=*/nullptr, image_info, /*out_ptr=*/nullptr);
        allocator.reset();
      },
      "VmaGpuAllocator destroyed with 3 live allocations");
}

}  // namespace
