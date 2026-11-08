// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <atomic>
#include <thread>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/ui/lib/escher/resources/resource_recycler.h"
#include "src/ui/lib/escher/test/common/gtest_escher.h"
#include "src/ui/lib/escher/test/common/gtest_vulkan.h"
#include "src/ui/lib/escher/test/vk/fake_gpu_allocator.h"
#include "src/ui/lib/escher/util/image_utils.h"
#include "src/ui/lib/escher/vk/command_buffer.h"
#include "src/ui/lib/escher/vk/image_factory.h"
#include "src/ui/lib/escher/vk/naive_gpu_allocator.h"
#include "src/ui/lib/escher/vk/vma_gpu_allocator.h"
#include "src/ui/lib/escher/vk/vulkan_context.h"
#include "src/ui/lib/escher/vk/vulkan_device_queues.h"

namespace {
using namespace escher;
using namespace escher::test;
using ::testing::_;

// Comparison macro used for checking bytes_allocated.
#define EXPECT_BETWEEN_INCL(x, left, right) EXPECT_TRUE((x) >= (left) && (x) <= (right))

// Don't allow too much wasted memory.
//
// As VmaAllocator now treats memory heap less than 256MB as "small heaps"
// (defined in escher/BUILD.gn), and will allocate 1/64 of heap size for all
// small memory heaps, we set kMaxUnusedMemory = 4MB so that it will work
// correctly on all devices.
const vk::DeviceSize kMaxUnusedMemory = 4u * 1024 * 1024;

VulkanDeviceQueuesPtr CreateVulkanDeviceQueues(bool use_protected_memory) {
  VulkanInstance::Params instance_params({{}, {VK_EXT_DEBUG_UTILS_EXTENSION_NAME}, false});

  auto validation_layer_name = VulkanInstance::GetValidationLayerName();
  if (validation_layer_name) {
    instance_params.layer_names.insert(*validation_layer_name);
  }

  auto vulkan_instance = VulkanInstance::New(std::move(instance_params));
  // This test doesn't use the global Escher environment so TestWithVkValidationLayer won't work.
  // We set up a custom debug callback function to fail the test when there is errors / warnings /
  // performance warnings.
  vulkan_instance->RegisterDebugUtilsMessengerCallback(
      [](VkDebugUtilsMessageSeverityFlagBitsEXT message_severity,
         VkDebugUtilsMessageTypeFlagsEXT message_types,
         const VkDebugUtilsMessengerCallbackDataEXT* callback_data, void* user_data) -> VkBool32 {
        uint64_t object =
            callback_data->objectCount == 0 ? 0ull : callback_data->pObjects[0].objectHandle;
        ADD_FAILURE() << "Debug report: "
                      << vk::to_string(vk::DebugUtilsMessageSeverityFlagsEXT(message_severity))
                      << " Object: " << object << " Layer: " << callback_data->pMessageIdName
                      << " Message code: " << callback_data->messageIdNumber
                      << " Message: " << callback_data->pMessage;
        return VK_FALSE;
      });

  VulkanDeviceQueues::Params::Flags flags =
      VulkanDeviceQueues::Params::kDisableQueueFilteringForPresent;
  if (use_protected_memory) {
    flags |= VulkanDeviceQueues::Params::kAllowProtectedMemory;
  }
  // This extension is necessary for the VMA to support dedicated allocations.
  auto vulkan_queues = VulkanDeviceQueues::New(
      vulkan_instance,
      {{VK_KHR_GET_MEMORY_REQUIREMENTS_2_EXTENSION_NAME}, {}, vk::SurfaceKHR(), flags});
  // Some devices might not be capable of using protected memory.
  if (use_protected_memory && !vulkan_queues->caps().allow_protected_memory) {
    return nullptr;
  }
  return vulkan_queues;
}

struct AllocationStat {
  int64_t bytes_allocated;
  int64_t unused_bytes_allocated;
  size_t total_bytes_allocated;
  size_t total_unused_bytes_allocated;
};

void UpdateAllocationCount(std::vector<AllocationStat>& allocation_stat_each_test,
                           GpuAllocator* allocator) {
  size_t total_bytes_allocated = allocator->GetTotalBytesAllocated();
  size_t total_unused_bytes_allocated = allocator->GetUnusedBytesAllocated();

  int64_t bytes_allocated = static_cast<int64_t>(total_bytes_allocated);
  int64_t unused_bytes_allocated = static_cast<int64_t>(total_unused_bytes_allocated);
  if (allocation_stat_each_test.size() >= 1) {
    bytes_allocated -= allocation_stat_each_test.back().total_bytes_allocated;
    unused_bytes_allocated -= allocation_stat_each_test.back().total_unused_bytes_allocated;
  }

  allocation_stat_each_test.push_back(
      {.bytes_allocated = bytes_allocated,
       .unused_bytes_allocated = unused_bytes_allocated,
       .total_bytes_allocated = total_bytes_allocated,
       .total_unused_bytes_allocated = total_unused_bytes_allocated});
}

void TestAllocationOfMemory(GpuAllocator* allocator) {
  // vk_mem_alloc allocates power of 2 buffers by default, so this makes the
  // tests easier to verify.
  constexpr vk::DeviceSize kMemorySize = 1024;
  constexpr vk::DeviceSize kMemorySizeAllowableError = 64;

  std::vector<AllocationStat> allocation_stat_each_test;

  // Confirm that all memory has been released.
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[0]
  EXPECT_EQ(0u, allocation_stat_each_test.back().total_bytes_allocated);
  EXPECT_EQ(0u, allocation_stat_each_test.back().total_unused_bytes_allocated);

  // Standard sub-allocation tests.
  auto alloc = allocator->AllocateMemory({kMemorySize, 0, 0xffffffff},
                                         vk::MemoryPropertyFlagBits::eDeviceLocal);

  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[1]

  // We ensure that for each allocation, the allocated byte size satisfies
  //   kMemorySize <= bytes_allocated <= kMemorySize + kMemorySizeAllowableError
  EXPECT_BETWEEN_INCL(static_cast<size_t>(allocation_stat_each_test.back().bytes_allocated),
                      kMemorySize, kMemorySize + kMemorySizeAllowableError);

  // Adding sub-allocations doesn't increase slab-count.
  auto sub_alloc1 = alloc->Suballocate(kMemorySize, 0);
  auto sub_alloc1a = sub_alloc1->Suballocate(kMemorySize, 0);
  auto sub_alloc1b = sub_alloc1->Suballocate(kMemorySize, 0);
  auto sub_alloc2 = alloc->Suballocate(kMemorySize, 0);
  auto sub_alloc2a = sub_alloc2->Suballocate(kMemorySize, 0);
  auto sub_alloc2b = sub_alloc2->Suballocate(kMemorySize, 0);

  // We expect that we didn't allocate any new memory.
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[2]
  EXPECT_EQ(0u, static_cast<size_t>(allocation_stat_each_test.back().bytes_allocated));
  EXPECT_EQ(0u, static_cast<size_t>(allocation_stat_each_test.back().unused_bytes_allocated));

  // Allocating then freeing increases/decreases the slab-count.
  auto alloc2 = allocator->AllocateMemory({kMemorySize, 0, 0xffffffff},
                                          vk::MemoryPropertyFlagBits::eHostVisible);

  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[3]

  // We ensure that for each allocation, the allocated byte size satisfies
  //   kMemorySize <= bytes_allocated <= kMemorySize + kMemorySizeAllowableError
  EXPECT_BETWEEN_INCL(static_cast<size_t>(allocation_stat_each_test.back().bytes_allocated),
                      kMemorySize, kMemorySize + kMemorySizeAllowableError);

  alloc2 = nullptr;
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[4]
  EXPECT_EQ(allocation_stat_each_test[2].total_bytes_allocated,
            allocation_stat_each_test.back().total_bytes_allocated);

  // Sub-allocations keep parent allocations alive.
  alloc = nullptr;
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[5]
  EXPECT_EQ(allocation_stat_each_test[1].total_bytes_allocated,
            allocation_stat_each_test.back().total_bytes_allocated);

  sub_alloc1 = nullptr;
  sub_alloc1a = nullptr;
  sub_alloc1b = nullptr;
  sub_alloc2 = nullptr;
  sub_alloc2a = nullptr;
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[6]
  EXPECT_EQ(allocation_stat_each_test[1].total_bytes_allocated,
            allocation_stat_each_test.back().total_bytes_allocated);

  sub_alloc2b = nullptr;
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[7]
  EXPECT_EQ(0u, allocation_stat_each_test.back().total_bytes_allocated);
}

void TestAllocationOfBuffers(GpuAllocator* allocator, bool expect_size_strictly_equal = true) {
  // vk_mem_alloc allocates power of 2 buffers by default, so this makes the
  // tests easier to verify.
  constexpr vk::DeviceSize kMemorySize = 1024;
  constexpr vk::DeviceSize kMemorySizeAllowableError = 64;

  std::vector<AllocationStat> allocation_stat_each_test;

  // Confirm that all memory has been released.
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[0]
  EXPECT_EQ(0u, allocation_stat_each_test.back().total_bytes_allocated);
  EXPECT_EQ(0u, allocation_stat_each_test.back().total_unused_bytes_allocated);

  const auto kBufferUsageFlags =
      vk::BufferUsageFlagBits::eTransferSrc | vk::BufferUsageFlagBits::eTransferDst;
  const auto kMemoryPropertyFlags =
      vk::MemoryPropertyFlagBits::eHostVisible | vk::MemoryPropertyFlagBits::eHostCoherent;

  // Allocate some buffers, and confirm that the allocator is tracking the bytes
  // allocated.
  auto buffer0 = allocator->AllocateBuffer(nullptr, kMemorySize, kBufferUsageFlags,
                                           kMemoryPropertyFlags, nullptr);
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[1]

  // We ensure that for each allocation, the allocated byte size satisfies
  //   kMemorySize <= bytes_allocated <= kMemorySize + kMemorySizeAllowableError
  EXPECT_BETWEEN_INCL(static_cast<size_t>(allocation_stat_each_test.back().bytes_allocated),
                      kMemorySize, kMemorySize + kMemorySizeAllowableError);

  EXPECT_NE(nullptr, buffer0->host_ptr());
  EXPECT_EQ(kMemorySize, buffer0->size());

  auto buffer1 = allocator->AllocateBuffer(nullptr, kMemorySize, kBufferUsageFlags,
                                           kMemoryPropertyFlags, nullptr);
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[2]
  EXPECT_LE(kMemorySize, static_cast<size_t>(allocation_stat_each_test.back().bytes_allocated));

  EXPECT_NE(nullptr, buffer1->host_ptr());
  EXPECT_EQ(kMemorySize, buffer1->size());

  // Allocate a buffer using dedicated memory and getting a separate managed
  // pointer to the memory.
  GpuMemPtr ptr;
  auto buffer_dedicated0 = allocator->AllocateBuffer(nullptr, kMemorySize, kBufferUsageFlags,
                                                     kMemoryPropertyFlags, &ptr);
  EXPECT_TRUE(ptr);
  if (expect_size_strictly_equal) {
    EXPECT_EQ(kMemorySize, ptr->size());
  } else {
    EXPECT_LE(kMemorySize, ptr->size());
  }
  EXPECT_EQ(0u, ptr->offset());
  EXPECT_NE(nullptr, ptr->mapped_ptr());
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[3]

  // We ensure that for each allocation, the allocated byte size satisfies
  //   kMemorySize <= bytes_allocated <= kMemorySize + kMemorySizeAllowableError
  EXPECT_BETWEEN_INCL(static_cast<size_t>(allocation_stat_each_test.back().bytes_allocated),
                      kMemorySize, kMemorySize + kMemorySizeAllowableError);

  // Release the objects, buffer first, and confirm that both need to be
  // destroyed before the memory is reclaimed.
  buffer_dedicated0 = nullptr;
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[4]
  EXPECT_EQ(allocation_stat_each_test[3].total_bytes_allocated,
            allocation_stat_each_test.back().total_bytes_allocated);

  ptr = nullptr;
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[5]
  EXPECT_EQ(allocation_stat_each_test[2].total_bytes_allocated,
            allocation_stat_each_test.back().total_bytes_allocated);

  // Allocate another dedicated memory object.
  buffer_dedicated0 = allocator->AllocateBuffer(nullptr, kMemorySize, kBufferUsageFlags,
                                                kMemoryPropertyFlags, &ptr);
  EXPECT_TRUE(ptr);
  if (expect_size_strictly_equal) {
    EXPECT_EQ(kMemorySize, ptr->size());
  } else {
    EXPECT_LE(kMemorySize, ptr->size());
  }
  EXPECT_EQ(0u, ptr->offset());
  EXPECT_NE(nullptr, ptr->mapped_ptr());
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[6]

  // We ensure that for each allocation, the allocated byte size satisfies
  //   kMemorySize <= bytes_allocated <= kMemorySize + kMemorySizeAllowableError
  EXPECT_BETWEEN_INCL(static_cast<size_t>(allocation_stat_each_test.back().bytes_allocated),
                      kMemorySize, kMemorySize + kMemorySizeAllowableError);

  // Release the objects in the opposite order, and perform the same test.
  ptr = nullptr;
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[7]
  EXPECT_EQ(allocation_stat_each_test[6].total_bytes_allocated,
            allocation_stat_each_test.back().total_bytes_allocated);

  buffer_dedicated0 = nullptr;
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[8]
  EXPECT_EQ(allocation_stat_each_test[5].total_bytes_allocated,
            allocation_stat_each_test.back().total_bytes_allocated);

  // Allocate non-power-of-two buffers, proving that, even though the allocator
  // could partition out a small pool, the requirement of an output memory
  // pointer forces unique allocations (i.e., offset == 0) for all objects.
  const auto kSmallBufferSize = 5u;
  auto buffer_dedicated1 = allocator->AllocateBuffer(nullptr, kSmallBufferSize, kBufferUsageFlags,
                                                     kMemoryPropertyFlags, &ptr);
  EXPECT_EQ(0u, ptr->offset());
  EXPECT_NE(nullptr, ptr->mapped_ptr());
  auto buffer_dedicated2 = allocator->AllocateBuffer(nullptr, kSmallBufferSize, kBufferUsageFlags,
                                                     kMemoryPropertyFlags, &ptr);
  EXPECT_EQ(0u, ptr->offset());
  EXPECT_NE(nullptr, ptr->mapped_ptr());
  auto buffer_dedicated3 = allocator->AllocateBuffer(nullptr, kSmallBufferSize, kBufferUsageFlags,
                                                     kMemoryPropertyFlags, &ptr);
  EXPECT_EQ(0u, ptr->offset());
  EXPECT_NE(nullptr, ptr->mapped_ptr());
  auto buffer_dedicated4 = allocator->AllocateBuffer(nullptr, kSmallBufferSize, kBufferUsageFlags,
                                                     kMemoryPropertyFlags, &ptr);
  EXPECT_EQ(0u, ptr->offset());
  EXPECT_NE(nullptr, ptr->mapped_ptr());
  auto buffer_dedicated5 = allocator->AllocateBuffer(nullptr, kSmallBufferSize, kBufferUsageFlags,
                                                     kMemoryPropertyFlags, &ptr);
  EXPECT_EQ(0u, ptr->offset());
  EXPECT_NE(nullptr, ptr->mapped_ptr());

  // Release all objects.
  buffer0 = nullptr;
  buffer1 = nullptr;
  buffer_dedicated1 = nullptr;
  buffer_dedicated2 = nullptr;
  buffer_dedicated3 = nullptr;
  buffer_dedicated4 = nullptr;
  buffer_dedicated5 = nullptr;
  ptr = nullptr;

  // Confirm that all memory has been released.
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[9]
  EXPECT_EQ(0u, allocation_stat_each_test.back().total_bytes_allocated);
}

void TestAllocationOfImages(GpuAllocator* allocator, bool use_protected_memory = false) {
  std::vector<AllocationStat> allocation_stat_each_test;

  // Confirm that all memory has been released.
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[0]
  EXPECT_EQ(0u, allocation_stat_each_test.back().total_bytes_allocated);
  EXPECT_EQ(0u, allocation_stat_each_test.back().total_unused_bytes_allocated);

  const auto kMemoryPropertyFlags = vk::MemoryPropertyFlagBits::eHostVisible;

  static const int kWidth = 64;
  static const int kHeight = 64;
  static const vk::Format kFormat = vk::Format::eR8G8B8A8Unorm;
  static const ColorSpace kColorSpace = escher::ColorSpace::kSrgb;
  static const size_t kMemorySize = kWidth * kHeight * image_utils::BytesPerPixel(kFormat);
  static const vk::DeviceSize kMemorySizeAllowableError = 128;
  static const vk::ImageUsageFlags kUsage =
      vk::ImageUsageFlagBits::eTransferSrc | vk::ImageUsageFlagBits::eTransferDst;

  ImageInfo info;
  info.format = kFormat;
  info.width = kWidth;
  info.height = kHeight;
  info.usage = kUsage;
  info.tiling = vk::ImageTiling::eLinear;
  info.color_space = kColorSpace;
  info.memory_flags = kMemoryPropertyFlags;
  if (use_protected_memory) {
    info.memory_flags = vk::MemoryPropertyFlagBits::eProtected;
  }

  // Allocate some images, and confirm that the allocator is tracking the bytes
  // allocated.
  auto image0 = allocator->AllocateImage(nullptr, info);
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[1]

  // We ensure that for each allocation, the allocated byte size satisfies
  //   kMemorySize <= bytes_allocated <= kMemorySize + kMemorySizeAllowableError
  EXPECT_BETWEEN_INCL(static_cast<size_t>(allocation_stat_each_test.back().bytes_allocated),
                      kMemorySize, kMemorySize + kMemorySizeAllowableError);

  // Protected memory should not be accessible by the host.
  EXPECT_TRUE(use_protected_memory || image0->host_ptr() != nullptr);
  EXPECT_BETWEEN_INCL(image0->size(), kMemorySize, kMemorySize + kMemorySizeAllowableError);

  auto image1 = allocator->AllocateImage(nullptr, info);

  EXPECT_TRUE(use_protected_memory || image1->host_ptr() != nullptr);
  EXPECT_BETWEEN_INCL(image1->size(), kMemorySize, kMemorySize + kMemorySizeAllowableError);
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[2]

  // We ensure that for each allocation, the allocated byte size satisfies
  //   kMemorySize <= bytes_allocated <= kMemorySize + kMemorySizeAllowableError
  EXPECT_BETWEEN_INCL(static_cast<size_t>(allocation_stat_each_test.back().bytes_allocated),
                      kMemorySize, kMemorySize + kMemorySizeAllowableError);

  // Allocate an image using dedicated memory and getting a separate managed
  // pointer to the memory.
  GpuMemPtr ptr;
  auto image_dedicated0 = allocator->AllocateImage(nullptr, info, &ptr);
  EXPECT_TRUE(ptr);
  EXPECT_BETWEEN_INCL(ptr->size(), kMemorySize, kMemorySize + kMemorySizeAllowableError);
  EXPECT_EQ(0u, ptr->offset());
  EXPECT_TRUE(use_protected_memory || ptr->mapped_ptr() != nullptr);
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[3]

  // We ensure that for each allocation, the allocated byte size satisfies
  //   kMemorySize <= bytes_allocated <= kMemorySize + kMemorySizeAllowableError
  EXPECT_BETWEEN_INCL(static_cast<size_t>(allocation_stat_each_test.back().bytes_allocated),
                      kMemorySize, kMemorySize + kMemorySizeAllowableError);

  // Release the objects, image first, and confirm that both need to be
  // destroyed before the memory is reclaimed.
  image_dedicated0 = nullptr;
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[4]
  EXPECT_EQ(allocation_stat_each_test[3].total_bytes_allocated,
            allocation_stat_each_test.back().total_bytes_allocated);

  ptr = nullptr;
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[5]
  EXPECT_EQ(allocation_stat_each_test[2].total_bytes_allocated,
            allocation_stat_each_test.back().total_bytes_allocated);

  // Allocate another dedicated memory object.
  image_dedicated0 = allocator->AllocateImage(nullptr, info, &ptr);
  EXPECT_TRUE(ptr);
  EXPECT_BETWEEN_INCL(ptr->size(), kMemorySize, kMemorySize + kMemorySizeAllowableError);
  EXPECT_EQ(0u, ptr->offset());
  EXPECT_TRUE(use_protected_memory || ptr->mapped_ptr() != nullptr);
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[6]

  // We ensure that for each allocation, the allocated byte size satisfies
  //   kMemorySize <= bytes_allocated <= kMemorySize + kMemorySizeAllowableError
  EXPECT_LE(kMemorySize, static_cast<size_t>(allocation_stat_each_test.back().bytes_allocated));
  EXPECT_GE(kMemorySize + kMemorySizeAllowableError,
            static_cast<size_t>(allocation_stat_each_test.back().bytes_allocated));

  // Release the objects in the opposite order, and perform the same test.
  ptr = nullptr;
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[7]
  EXPECT_EQ(allocation_stat_each_test[6].total_bytes_allocated,
            allocation_stat_each_test.back().total_bytes_allocated);
  image_dedicated0 = nullptr;
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[8]
  EXPECT_EQ(allocation_stat_each_test[5].total_bytes_allocated,
            allocation_stat_each_test.back().total_bytes_allocated);

  // Allocate non-power-of-two buffers, proving that, even though the allocator
  // could partition out a small pool, the requirement of an output memory
  // pointer forces unique allocations (i.e., offset == 0) for all objects.
  ImageInfo small_image;
  small_image.format = kFormat;
  small_image.width = 1;
  small_image.height = 1;
  small_image.usage = kUsage;
  small_image.tiling = vk::ImageTiling::eLinear;
  small_image.color_space = kColorSpace;
  small_image.memory_flags = kMemoryPropertyFlags;

  auto image_dedicated1 = allocator->AllocateImage(nullptr, small_image, &ptr);
  EXPECT_EQ(0u, ptr->offset());
  EXPECT_NE(nullptr, ptr->mapped_ptr());
  auto image_dedicated2 = allocator->AllocateImage(nullptr, small_image, &ptr);
  EXPECT_EQ(0u, ptr->offset());
  EXPECT_NE(nullptr, ptr->mapped_ptr());
  auto image_dedicated3 = allocator->AllocateImage(nullptr, small_image, &ptr);
  EXPECT_EQ(0u, ptr->offset());
  EXPECT_NE(nullptr, ptr->mapped_ptr());
  auto image_dedicated4 = allocator->AllocateImage(nullptr, small_image, &ptr);
  EXPECT_EQ(0u, ptr->offset());
  EXPECT_NE(nullptr, ptr->mapped_ptr());
  auto image_dedicated5 = allocator->AllocateImage(nullptr, small_image, &ptr);
  EXPECT_EQ(0u, ptr->offset());
  EXPECT_NE(nullptr, ptr->mapped_ptr());

  // Release all objects.
  image0 = nullptr;
  image1 = nullptr;
  image_dedicated1 = nullptr;
  image_dedicated2 = nullptr;
  image_dedicated3 = nullptr;
  image_dedicated4 = nullptr;
  image_dedicated5 = nullptr;
  ptr = nullptr;
  // Confirm that all memory has been released.
  UpdateAllocationCount(allocation_stat_each_test, allocator);  // Set allocation_stat_each_test[9]
  EXPECT_EQ(0u, allocation_stat_each_test.back().total_bytes_allocated);
}

// The fake allocator is intended to be used when there is not a valid Vulkan
// instance. So this TEST should not need to be upgraded to a VK_TEST.
TEST(FakeAllocator, Memory) {
  FakeGpuAllocator allocator;

  TestAllocationOfMemory(&allocator);
}

TEST(FakeAllocator, Buffers) {
  FakeGpuAllocator allocator;

  TestAllocationOfBuffers(&allocator);
}

TEST(FakeAllocator, Images) {
  FakeGpuAllocator allocator;

  TestAllocationOfImages(&allocator);
}

// These tests check real Vulkan allocators, so they have a true dependency on
// Vulkan.
VK_TEST(NaiveAllocator, NaiveAllocator) {
  auto vulkan_queues = CreateVulkanDeviceQueues(false);
  NaiveGpuAllocator allocator(vulkan_queues->GetVulkanContext());

  TestAllocationOfMemory(&allocator);

  // TODO(https://fxbug.dev/42152112): This test crashes because we pass a null ResourceManager into
  // GpuAllocator. Creating a ResourceManager requires a functional Escher
  // object, but this test only needs a VulkanContext. This bug tracks
  // simplifying the dependency chain, so that all we need is a VulkanContext,
  // which we do have in this unit test.

  // TestAllocationOfBuffers(&allocator);
  // TestAllocationOfImages(&allocator);
}

class VmaAllocator : public ::testing::TestWithParam</*protected_memory=*/bool> {};

VK_TEST_P(VmaAllocator, Memory) {
  auto vulkan_queues = CreateVulkanDeviceQueues(GetParam());
  if (!vulkan_queues) {
    GTEST_SKIP();
  }
  VmaGpuAllocator allocator(vulkan_queues->GetVulkanContext());

  TestAllocationOfMemory(&allocator);
}

VK_TEST_P(VmaAllocator, Buffers) {
  auto vulkan_queues = CreateVulkanDeviceQueues(GetParam());
  if (!vulkan_queues) {
    GTEST_SKIP();
  }
  VmaGpuAllocator allocator(vulkan_queues->GetVulkanContext());

  // Some Vulkan devices (e.g. SwiftShaderf) may require a memory size larger
  // than the expected size in tests. We should allow for this case when using
  // VMA memory allocator.
  TestAllocationOfBuffers(&allocator, /* expect_size_strictly_equal= */ false);
}

VK_TEST_P(VmaAllocator, Images) {
  auto vulkan_queues = CreateVulkanDeviceQueues(GetParam());
  if (!vulkan_queues) {
    GTEST_SKIP();
  }
  VmaGpuAllocator allocator(vulkan_queues->GetVulkanContext());

  TestAllocationOfImages(&allocator, GetParam());
}

INSTANTIATE_TEST_SUITE_P(VmaAllocatorTestSuite, VmaAllocator, ::testing::Bool());

class MockVmaGpuAllocator : public VmaGpuAllocator {
 public:
  MockVmaGpuAllocator(const VulkanContext& context) : VmaGpuAllocator(context) {}

  MOCK_METHOD(bool, CreateImage,
              (const VkImageCreateInfo& image_create_info,
               const VmaAllocationCreateInfo& allocation_create_info, VkImage* image,
               VmaAllocation* vma_allocation, VmaAllocationInfo* vma_allocation_info));
};

VK_TEST(VmaGpuAllocatorTest, ProtectedMemoryIsDedicated) {
  auto vulkan_queues = CreateVulkanDeviceQueues(/*use_protected_memory=*/true);
  if (!vulkan_queues) {
    GTEST_SKIP();
  }
  MockVmaGpuAllocator allocator(vulkan_queues->GetVulkanContext());

  VmaAllocationCreateInfo allocation_create_info;
  EXPECT_CALL(allocator, CreateImage(_, _, _, _, _))
      .Times(1)
      .WillOnce(::testing::DoAll(::testing::SaveArg<1>(&allocation_create_info),
                                 ::testing::Return(false)));
  ImageInfo info;
  info.format = vk::Format::eB8G8R8A8Unorm;
  info.usage = vk::ImageUsageFlagBits::eTransferDst;
  info.color_space = escher::ColorSpace::kSrgb;
  info.memory_flags = vk::MemoryPropertyFlagBits::eProtected;
  auto image0 = allocator.AllocateImage(nullptr, info, nullptr);
  EXPECT_TRUE(allocation_create_info.flags & VMA_ALLOCATION_CREATE_DEDICATED_MEMORY_BIT);
}

using ImageFactoryAdapterTest = TestWithVkValidationLayer;

VK_TEST_F(ImageFactoryAdapterTest, ImmediateAndDedicatedAllocationLifecycle) {
  auto* escher = test::GetEscher();
  EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
  EXPECT_TRUE(escher->Cleanup());
  const uint32_t baseline_ownable_count = escher->resource_recycler()->ownable_count();

  FakeGpuAllocator allocator;
  ResourceRecycler recycler(escher->GetWeakPtr());
  ImageFactoryAdapter adapter(&allocator, &recycler);

  constexpr uint32_t kWidth = 16;
  constexpr uint32_t kHeight = 16;
  constexpr vk::Format kFormat = vk::Format::eR8G8B8A8Unorm;
  const size_t kMemorySize = kWidth * kHeight * image_utils::BytesPerPixel(kFormat);

  ImageInfo info;
  info.format = kFormat;
  info.width = kWidth;
  info.height = kHeight;
  info.usage = vk::ImageUsageFlagBits::eColorAttachment | vk::ImageUsageFlagBits::eSampled;
  info.color_space = ColorSpace::kSrgb;

  // An image that is never referenced by a command buffer (`sequence_number() == 0`) is
  // destroyed immediately when its last `ImagePtr` reference is dropped.
  auto image = adapter.NewImage(info);
  ASSERT_TRUE(image);
  EXPECT_EQ(0u, image->sequence_number());
  EXPECT_EQ(kMemorySize, allocator.GetTotalBytesAllocated());
  EXPECT_EQ(1u, recycler.ownable_count());

  image = nullptr;
  EXPECT_EQ(0u, allocator.GetTotalBytesAllocated());
  EXPECT_EQ(0u, recycler.ownable_count());

  // Dedicated allocation (`out_ptr != nullptr`) populates `out_mem` and keeps the underlying
  // `GpuMem` alive until both the `ImagePtr` and `out_mem` are dropped.
  GpuMemPtr out_mem;
  image = adapter.NewImage(info, &out_mem);
  ASSERT_TRUE(image);
  ASSERT_TRUE(out_mem);
  EXPECT_EQ(kMemorySize, out_mem->size());
  EXPECT_EQ(kMemorySize, allocator.GetTotalBytesAllocated());
  EXPECT_EQ(1u, recycler.ownable_count());

  // Dropping `image` first destroys the `Image` resource immediately, while `out_mem` keeps the
  // backing memory allocation alive.
  image = nullptr;
  EXPECT_EQ(0u, recycler.ownable_count());
  EXPECT_EQ(kMemorySize, allocator.GetTotalBytesAllocated());

  out_mem = nullptr;
  EXPECT_EQ(0u, allocator.GetTotalBytesAllocated());

  // Verify the reverse drop order: dropping `out_mem` first keeps the backing memory alive
  // until `image` is also dropped.
  image = adapter.NewImage(info, &out_mem);
  ASSERT_TRUE(image);
  ASSERT_TRUE(out_mem);
  EXPECT_EQ(kMemorySize, allocator.GetTotalBytesAllocated());
  EXPECT_EQ(1u, recycler.ownable_count());

  out_mem = nullptr;
  EXPECT_EQ(kMemorySize, allocator.GetTotalBytesAllocated());
  EXPECT_EQ(1u, recycler.ownable_count());

  image = nullptr;
  EXPECT_EQ(0u, allocator.GetTotalBytesAllocated());
  EXPECT_EQ(0u, recycler.ownable_count());

  // Also exercise dedicated allocation through `Escher::image_cache()` (backed by
  // `VmaGpuAllocator` and `escher->resource_recycler()`). Because `VmaMappedGpuMem` holds a
  // `keep_alive_` reference to the `VmaImage`, `ownable_count()` remains elevated until both
  // the `ImagePtr` and `vma_out_mem` are dropped.
  GpuMemPtr vma_out_mem;
  auto vma_image = escher->image_cache()->NewImage(info, &vma_out_mem);
  ASSERT_TRUE(vma_image);
  ASSERT_TRUE(vma_out_mem);
  EXPECT_GE(vma_out_mem->size(), kMemorySize);
  EXPECT_EQ(baseline_ownable_count + 1u, escher->resource_recycler()->ownable_count());

  vma_image = nullptr;
  EXPECT_EQ(baseline_ownable_count + 1u, escher->resource_recycler()->ownable_count());

  vma_out_mem = nullptr;
  EXPECT_EQ(baseline_ownable_count, escher->resource_recycler()->ownable_count());

  // Reverse drop order for `Escher::image_cache()`: drop `vma_out_mem` first, then `vma_image`.
  vma_image = escher->image_cache()->NewImage(info, &vma_out_mem);
  ASSERT_TRUE(vma_image);
  ASSERT_TRUE(vma_out_mem);
  EXPECT_EQ(baseline_ownable_count + 1u, escher->resource_recycler()->ownable_count());

  vma_out_mem = nullptr;
  EXPECT_EQ(baseline_ownable_count + 1u, escher->resource_recycler()->ownable_count());

  vma_image = nullptr;
  EXPECT_EQ(baseline_ownable_count, escher->resource_recycler()->ownable_count());
}

VK_TEST_F(ImageFactoryAdapterTest, InFlightImageDeferredUntilCommandBufferFinishes) {
  auto* escher = test::GetEscher();
  EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
  EXPECT_TRUE(escher->Cleanup());

  const uint32_t baseline_ownable_count = escher->resource_recycler()->ownable_count();

  constexpr uint32_t kWidth = 16;
  constexpr uint32_t kHeight = 16;
  constexpr vk::Format kFormat = vk::Format::eR8G8B8A8Unorm;
  const size_t kMemorySize = kWidth * kHeight * image_utils::BytesPerPixel(kFormat);

  ImageInfo info;
  info.format = kFormat;
  info.width = kWidth;
  info.height = kHeight;
  info.usage = vk::ImageUsageFlagBits::eColorAttachment | vk::ImageUsageFlagBits::eTransferDst;
  info.memory_flags = vk::MemoryPropertyFlagBits::eDeviceLocal;
  info.color_space = ColorSpace::kSrgb;

  auto image1 = escher->image_cache()->NewImage(info);
  ASSERT_TRUE(image1);
  EXPECT_EQ(baseline_ownable_count + 1u, escher->resource_recycler()->ownable_count());

  auto cb = CommandBuffer::NewForGraphics(escher, /*use_protected_memory=*/false);
  ASSERT_TRUE(cb);
  cb->TransitionImageLayout(image1, vk::ImageLayout::eUndefined,
                            vk::ImageLayout::eColorAttachmentOptimal);
  EXPECT_EQ(cb->sequence_number(), image1->sequence_number());

  // Dropping the last `ImagePtr` while `cb` is still in flight must defer destruction in
  // `ResourceRecycler` rather than destroying or reusing the active image.
  const Image* image1_raw = image1.get();
  image1 = nullptr;
  EXPECT_EQ(baseline_ownable_count + 1u, escher->resource_recycler()->ownable_count());

  // A subsequent `NewImage(info)` call while `cb` is in flight must allocate a distinct `Image`
  // rather than aliasing `image1`.
  auto image2 = escher->image_cache()->NewImage(info);
  ASSERT_TRUE(image2);
  EXPECT_NE(image1_raw, image2.get());
  EXPECT_EQ(baseline_ownable_count + 2u, escher->resource_recycler()->ownable_count());

  // Also track exact byte deallocation timing via `FakeGpuAllocator` paired with a local
  // `ResourceRecycler`.
  FakeGpuAllocator fake_allocator;
  ResourceRecycler fake_recycler(escher->GetWeakPtr());
  ImageFactoryAdapter fake_adapter(&fake_allocator, &fake_recycler);
  auto fake_image = fake_adapter.NewImage(info);
  ASSERT_TRUE(fake_image);
  EXPECT_EQ(kMemorySize, fake_allocator.GetTotalBytesAllocated());
  EXPECT_EQ(1u, fake_recycler.ownable_count());

  cb->KeepAlive(fake_image);
  fake_image = nullptr;
  EXPECT_EQ(kMemorySize, fake_allocator.GetTotalBytesAllocated());
  EXPECT_EQ(1u, fake_recycler.ownable_count());

  // Submitting and retiring `cb` destroys the deferred `image1` and `fake_image`, while `image2`
  // remains alive.
  EXPECT_TRUE(cb->Submit(nullptr));
  EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
  EXPECT_TRUE(escher->Cleanup());

  EXPECT_EQ(0u, fake_allocator.GetTotalBytesAllocated());
  EXPECT_EQ(0u, fake_recycler.ownable_count());
  EXPECT_EQ(baseline_ownable_count + 1u, escher->resource_recycler()->ownable_count());

  image2 = nullptr;
  EXPECT_EQ(baseline_ownable_count, escher->resource_recycler()->ownable_count());
}

VK_TEST_F(ImageFactoryAdapterTest, ConcurrentInFlightImageReleaseAcrossThreads) {
  auto* escher = test::GetEscher();
  EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
  EXPECT_TRUE(escher->Cleanup());

  constexpr uint32_t kWidth = 16;
  constexpr uint32_t kHeight = 16;
  constexpr vk::Format kFormat = vk::Format::eR8G8B8A8Unorm;
  const size_t kMemorySize = kWidth * kHeight * image_utils::BytesPerPixel(kFormat);

  ImageInfo info;
  info.format = kFormat;
  info.width = kWidth;
  info.height = kHeight;
  info.usage = vk::ImageUsageFlagBits::eColorAttachment | vk::ImageUsageFlagBits::eSampled;
  info.color_space = ColorSpace::kSrgb;

  auto cb = CommandBuffer::NewForGraphics(escher, /*use_protected_memory=*/false);
  ASSERT_TRUE(cb);
  const uint64_t cb_seq = cb->sequence_number();
  ASSERT_GT(cb_seq, 0u);

  FakeGpuAllocator allocator;
  ResourceRecycler recycler(escher->GetWeakPtr());
  ImageFactoryAdapter adapter(&allocator, &recycler);

  constexpr size_t kNumThreads = 4;
  constexpr size_t kImagesPerThread = 32;
  constexpr size_t kTotalImagesPerCategory = kNumThreads * kImagesPerThread;

  // Pre-allocate both in-flight (`sequence_number() == cb_seq`) and unreferenced
  // (`sequence_number() == 0`) images on the main thread so worker threads do not dereference
  // thread-affine `fxl::WeakPtr` instances (`ImageFactoryAdapter::allocator_` and
  // `ResourceManager::escher_`) off-thread.
  std::vector<std::vector<ImagePtr>> in_flight_images(kNumThreads);
  std::vector<std::vector<ImagePtr>> unreferenced_images(kNumThreads);
  for (size_t t = 0; t < kNumThreads; ++t) {
    in_flight_images[t].reserve(kImagesPerThread);
    unreferenced_images[t].reserve(kImagesPerThread);
    for (size_t i = 0; i < kImagesPerThread; ++i) {
      auto in_flight_img = adapter.NewImage(info);
      ASSERT_TRUE(in_flight_img);
      cb->KeepAlive(in_flight_img);
      EXPECT_EQ(cb_seq, in_flight_img->sequence_number());
      in_flight_images[t].push_back(std::move(in_flight_img));

      auto unreferenced_img = adapter.NewImage(info);
      ASSERT_TRUE(unreferenced_img);
      EXPECT_EQ(0u, unreferenced_img->sequence_number());
      unreferenced_images[t].push_back(std::move(unreferenced_img));
    }
  }

  EXPECT_EQ(2 * kTotalImagesPerCategory * kMemorySize, allocator.GetTotalBytesAllocated());
  EXPECT_EQ(2 * kTotalImagesPerCategory, recycler.ownable_count());

  // Gate worker threads on a start barrier so all threads contend concurrently on
  // `ResourceRecycler::OnReceiveOwnable` (both the deferred `lock_` path and the immediate
  // destruction path).
  std::atomic<size_t> ready_workers{0};
  std::atomic<bool> start{false};
  std::vector<std::thread> threads;
  threads.reserve(kNumThreads);
  for (size_t t = 0; t < kNumThreads; ++t) {
    threads.emplace_back([&, t]() {
      ready_workers.fetch_add(1, std::memory_order_release);
      while (!start.load(std::memory_order_acquire)) {
        std::this_thread::yield();
      }
      for (size_t i = 0; i < kImagesPerThread; ++i) {
        in_flight_images[t][i] = nullptr;
        unreferenced_images[t][i] = nullptr;
      }
    });
  }

  while (ready_workers.load(std::memory_order_acquire) < kNumThreads) {
    std::this_thread::yield();
  }
  start.store(true, std::memory_order_release);
  for (auto& thread : threads) {
    thread.join();
  }

  // All unreferenced images were destroyed immediately, while all in-flight images remain
  // retained in `recycler` waiting for `cb_seq` to finish.
  EXPECT_EQ(kTotalImagesPerCategory * kMemorySize, allocator.GetTotalBytesAllocated());
  EXPECT_EQ(kTotalImagesPerCategory, recycler.ownable_count());

  // Submitting and retiring `cb` notifies `recycler` via `CommandBufferSequencer` and destroys
  // all deferred in-flight images.
  EXPECT_TRUE(cb->Submit(nullptr));
  EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
  EXPECT_TRUE(escher->Cleanup());

  EXPECT_EQ(0u, allocator.GetTotalBytesAllocated());
  EXPECT_EQ(0u, recycler.ownable_count());
}

}  // namespace
