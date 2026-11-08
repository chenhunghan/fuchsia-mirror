// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <thread>
#include <vector>

#include <gtest/gtest.h>
#include <src/performance/memory/sampler/instrumentation/allocation_table.h>

namespace memory_sampler {

struct AllocationTableTestPeer {
  static constexpr size_t kWaysPerBucket = AllocationTable::kWaysPerBucket;

  static size_t GetBucketIndex(void* address) {
    return AllocationTable::GetBucketIndex(reinterpret_cast<uintptr_t>(address));
  }

  static size_t GetOverflowCount(const AllocationTable& table) {
    return table.overflow_count_.load(std::memory_order_relaxed);
  }
};

namespace {

// NOLINTNEXTLINE(performance-no-int-to-ptr)
void* const kTestAddress = reinterpret_cast<void*>(0x1000);
// NOLINTNEXTLINE(performance-no-int-to-ptr)
void* const kOtherAddress = reinterpret_cast<void*>(0x2000);

TEST(AllocationTableTest, InsertAndTryRemoveSingleAddress) {
  AllocationTable table;
  EXPECT_FALSE(table.TryRemove(kTestAddress));

  table.Insert(kTestAddress);
  EXPECT_FALSE(table.TryRemove(kOtherAddress));
  EXPECT_TRUE(table.TryRemove(kTestAddress));
  EXPECT_FALSE(table.TryRemove(kTestAddress));
}

TEST(AllocationTableTest, NullptrIsIgnored) {
  AllocationTable table;
  table.Insert(nullptr);
  EXPECT_FALSE(table.TryRemove(nullptr));
  EXPECT_EQ(AllocationTableTestPeer::GetOverflowCount(table), 0u);
}

TEST(AllocationTableTest, ReinsertAfterRemoveSucceeds) {
  AllocationTable table;
  table.Insert(kTestAddress);
  EXPECT_TRUE(table.TryRemove(kTestAddress));
  EXPECT_FALSE(table.TryRemove(kTestAddress));

  table.Insert(kTestAddress);
  EXPECT_TRUE(table.TryRemove(kTestAddress));
  EXPECT_FALSE(table.TryRemove(kTestAddress));
}

TEST(AllocationTableTest, BucketOverflowSpillsAndRecovers) {
  AllocationTable table;

  constexpr size_t kOverflowEntries = 4;
  constexpr size_t kTotalRecorded = AllocationTableTestPeer::kWaysPerBucket + kOverflowEntries;
  const size_t target_bucket = AllocationTableTestPeer::GetBucketIndex(kTestAddress);

  std::vector<void*> colliding_addresses;
  void* unrecorded_colliding_address = nullptr;
  for (uintptr_t candidate = 0x10000; colliding_addresses.size() < kTotalRecorded + 1;
       candidate += 16) {
    // NOLINTNEXTLINE(performance-no-int-to-ptr)
    void* addr = reinterpret_cast<void*>(candidate);
    if (AllocationTableTestPeer::GetBucketIndex(addr) == target_bucket) {
      if (colliding_addresses.size() < kTotalRecorded) {
        colliding_addresses.push_back(addr);
      } else {
        unrecorded_colliding_address = addr;
        break;
      }
    }
  }
  ASSERT_EQ(colliding_addresses.size(), kTotalRecorded);
  ASSERT_NE(unrecorded_colliding_address, nullptr);

  // Fill the bucket ways without spilling into the overflow set.
  for (size_t i = 0; i < AllocationTableTestPeer::kWaysPerBucket; ++i) {
    table.Insert(colliding_addresses[i]);
  }
  EXPECT_EQ(AllocationTableTestPeer::GetOverflowCount(table), 0u);

  // Additional colliding allocations spill into the overflow set.
  for (size_t i = AllocationTableTestPeer::kWaysPerBucket; i < kTotalRecorded; ++i) {
    table.Insert(colliding_addresses[i]);
  }
  EXPECT_EQ(AllocationTableTestPeer::GetOverflowCount(table), kOverflowEntries);

  // Removing an unrecorded address in the same bucket returns false without altering state.
  EXPECT_FALSE(table.TryRemove(unrecorded_colliding_address));
  EXPECT_EQ(AllocationTableTestPeer::GetOverflowCount(table), kOverflowEntries);

  // Removing all colliding addresses (both inline ways and overflow entries) succeeds once each.
  for (void* addr : colliding_addresses) {
    EXPECT_TRUE(table.TryRemove(addr));
    EXPECT_FALSE(table.TryRemove(addr));
  }
  EXPECT_EQ(AllocationTableTestPeer::GetOverflowCount(table), 0u);
}

TEST(AllocationTableTest, ClearRemovesBucketAndOverflowEntries) {
  AllocationTable table;

  constexpr size_t kTotalRecorded = AllocationTableTestPeer::kWaysPerBucket + 3;
  const size_t target_bucket = AllocationTableTestPeer::GetBucketIndex(kTestAddress);

  std::vector<void*> colliding_addresses;
  for (uintptr_t candidate = 0x10000; colliding_addresses.size() < kTotalRecorded;
       candidate += 16) {
    // NOLINTNEXTLINE(performance-no-int-to-ptr)
    void* addr = reinterpret_cast<void*>(candidate);
    if (AllocationTableTestPeer::GetBucketIndex(addr) == target_bucket) {
      colliding_addresses.push_back(addr);
      table.Insert(addr);
    }
  }
  ASSERT_EQ(AllocationTableTestPeer::GetOverflowCount(table), 3u);

  table.Clear();
  EXPECT_EQ(AllocationTableTestPeer::GetOverflowCount(table), 0u);
  for (void* addr : colliding_addresses) {
    EXPECT_FALSE(table.TryRemove(addr));
  }
}

TEST(AllocationTableTest, ConcurrentInsertAndTryRemove) {
  AllocationTable table;

  constexpr size_t kNumThreads = 8;
  constexpr size_t kAddressesPerThread = 4;
  constexpr size_t kIterations = 200;
  constexpr size_t kTotalColliding = kNumThreads * (kAddressesPerThread + 1);
  const size_t target_bucket = AllocationTableTestPeer::GetBucketIndex(kTestAddress);

  std::vector<void*> colliding_addresses;
  colliding_addresses.reserve(kTotalColliding);
  for (uintptr_t candidate = 0x10000; colliding_addresses.size() < kTotalColliding;
       candidate += 16) {
    // NOLINTNEXTLINE(performance-no-int-to-ptr)
    void* addr = reinterpret_cast<void*>(candidate);
    if (AllocationTableTestPeer::GetBucketIndex(addr) == target_bucket) {
      colliding_addresses.push_back(addr);
    }
  }
  ASSERT_EQ(colliding_addresses.size(), kTotalColliding);

  std::atomic<bool> start{false};
  std::vector<std::thread> threads;
  threads.reserve(kNumThreads);
  for (size_t t = 0; t < kNumThreads; ++t) {
    threads.emplace_back([&, t]() {
      const size_t base = t * (kAddressesPerThread + 1);
      void* const unrecorded = colliding_addresses[base + kAddressesPerThread];
      while (!start.load(std::memory_order_acquire)) {
      }
      for (size_t iter = 0; iter < kIterations; ++iter) {
        for (size_t i = 0; i < kAddressesPerThread; ++i) {
          table.Insert(colliding_addresses[base + i]);
        }
        EXPECT_FALSE(table.TryRemove(unrecorded));
        for (size_t i = 0; i < kAddressesPerThread; ++i) {
          EXPECT_TRUE(table.TryRemove(colliding_addresses[base + i]));
          EXPECT_FALSE(table.TryRemove(colliding_addresses[base + i]));
        }
      }
    });
  }

  start.store(true, std::memory_order_release);
  for (auto& thread : threads) {
    thread.join();
  }

  EXPECT_EQ(AllocationTableTestPeer::GetOverflowCount(table), 0u);
  for (void* addr : colliding_addresses) {
    EXPECT_FALSE(table.TryRemove(addr));
  }
}

}  // namespace
}  // namespace memory_sampler
