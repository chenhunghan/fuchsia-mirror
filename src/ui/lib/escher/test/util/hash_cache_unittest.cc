// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/lib/escher/util/hash_cache.h"

#include <gtest/gtest.h>

namespace {
using namespace escher;

class TestObj : public HashCacheItem<TestObj> {
 public:
  TestObj(size_t id) : id_(id) {}

  size_t id() const { return id_; }

 private:
  size_t id_;
};

// Block-based allocation policy.  This could be written more straightforwardly,
// but is written this way to explore the nuances of policies that clients might
// want to provide.
class TestObjObjectPoolPolicy {
 public:
  TestObjObjectPoolPolicy() {}
  ~TestObjObjectPoolPolicy() {
    EXPECT_EQ(init_object_count_, destroy_object_count_);
    EXPECT_EQ(init_block_count_, destroy_block_count_);
  }

  // Objects are constructed in InitializePoolObjectBlock(), so there is no need
  // to do it here.  See DescriptorSetAllocator for a real-life example of why
  // a client might create/destroy blocks of objects, rather than each object
  // separately.
  inline void InitializePoolObject(TestObj* ptr) { ++init_object_count_; }

  // HashCache conveniently wraps whatever policy is provided to it with a
  // HashCacheObjectPoolPolicy.  Its version of DestroyPoolObject() doesn't
  // call the destructor (because then the object state would be undefined the
  // next time InitializePoolObject() is called), but it does clear the various
  // fields used by the HashCache implementation.
  inline void DestroyPoolObject(TestObj* ptr) { ++destroy_object_count_; }

  // Construct an entire block of objects.
  void InitializePoolObjectBlock(TestObj* objects, size_t block_index, size_t num_objects) {
    ++init_block_count_;

    ASSERT_EQ(num_objects, ObjectPool<TestObj>::NumObjectsInBlock(block_index));

    size_t i, base_index = 0;
    for (i = 0; i < block_index; ++i) {
      base_index += ObjectPool<TestObj>::NumObjectsInBlock(i);
    }

    for (i = 0; i < num_objects; ++i) {
      // Cause IDs to be contiguous when ObjectPool immediately adds these to
      // |vacants_|.
      new (objects + i) TestObj(base_index + num_objects - i - 1);
    }
  }

  // We don't actually call destructors for the TestObjs.  This is OK because we
  // know that TestObj doesn't hold any resouces that can be leaked.  For many
  // use-cases, this will not suffice.  For example, DescriptorSetAllocator
  // would leak Vulkan objects if it followed this approach.
  void DestroyPoolObjectBlock(TestObj* objects, size_t block_index, size_t num_objects) {
    ++destroy_block_count_;
  }

  size_t init_object_count() const { return init_object_count_; }
  size_t destroy_object_count() const { return destroy_object_count_; }
  size_t init_block_count() const { return init_block_count_; }
  size_t destroy_block_count() const { return destroy_block_count_; }

 private:
  size_t init_object_count_ = 0;
  size_t destroy_object_count_ = 0;
  size_t init_block_count_ = 0;
  size_t destroy_block_count_ = 0;
};

// Helper function for the test-cases below.
template <class CacheT>
void ObtainAndValidateObjects(CacheT* cache, bool already_cached, size_t count,
                              size_t start_index = 0) {
  for (size_t i = start_index; i < start_index + count; ++i) {
    auto result = cache->Obtain({i});
    ASSERT_EQ(result.second, already_cached);
    ASSERT_EQ(i, result.first->id());
  }
}

// Test HashCache with |FramesUntilEviction| == 0, which means that there is no
// frame-to-frame caching: objects are not cached even if they were used in the
// previous frame.
TEST(HashCache, NoFrameToFrameCaching) {
  constexpr size_t kCount = 512;
  constexpr size_t kFramesUntilEviction = 0;
  HashCache<TestObj, TestObjObjectPoolPolicy, kFramesUntilEviction> cache;
  auto& policy = cache.object_pool().policy();

  cache.BeginFrame();
  EXPECT_EQ(0U, policy.init_object_count());
  EXPECT_EQ(0U, policy.destroy_object_count());
  EXPECT_EQ(0U, policy.init_block_count());
  EXPECT_EQ(0U, policy.destroy_block_count());

  ObtainAndValidateObjects(&cache, false, kCount);
  EXPECT_EQ(kCount, policy.init_object_count());
  EXPECT_EQ(0U, policy.destroy_object_count());
  EXPECT_EQ(4U, policy.init_block_count());
  EXPECT_EQ(0U, policy.destroy_block_count());

  // Accessing the same keys as before will not increment the block count.
  ObtainAndValidateObjects(&cache, true, kCount);
  ObtainAndValidateObjects(&cache, true, kCount);
  EXPECT_EQ(kCount, policy.init_object_count());
  EXPECT_EQ(0U, policy.destroy_object_count());
  EXPECT_EQ(4U, policy.init_block_count());
  EXPECT_EQ(0U, policy.destroy_block_count());

  // All the current objects are evicted from the cache upon BeginFrame().
  // However, the underlying memory is not freed; it remains available for
  // subsequent allocation requests.
  cache.BeginFrame();
  EXPECT_EQ(kCount, policy.init_object_count());
  EXPECT_EQ(kCount, policy.destroy_object_count());
  EXPECT_EQ(4U, policy.init_block_count());
  EXPECT_EQ(0U, policy.destroy_block_count());

  ObtainAndValidateObjects(&cache, false, kCount);
  EXPECT_EQ(2 * kCount, policy.init_object_count());
  EXPECT_EQ(kCount, policy.destroy_object_count());
  EXPECT_EQ(4U, policy.init_block_count());
  EXPECT_EQ(0U, policy.destroy_block_count());

  ObtainAndValidateObjects(&cache, true, kCount);
  ObtainAndValidateObjects(&cache, true, kCount);
  EXPECT_EQ(2 * kCount, policy.init_object_count());
  EXPECT_EQ(kCount, policy.destroy_object_count());
  EXPECT_EQ(4U, policy.init_block_count());
  EXPECT_EQ(0U, policy.destroy_block_count());

  // Clear() destroys all objects/blocks.  Don't do this until you are sure
  // that the objects are no longer being used.  For example, if the cache holds
  // Vulkan objects, don't destroy them while they are still being used to
  // render a frame.
  cache.Clear();
  EXPECT_EQ(2 * kCount, policy.init_object_count());
  EXPECT_EQ(2 * kCount, policy.destroy_object_count());
  EXPECT_EQ(4U, policy.init_block_count());
  EXPECT_EQ(4U, policy.destroy_block_count());

  ObtainAndValidateObjects(&cache, false, kCount);
  EXPECT_EQ(3 * kCount, policy.init_object_count());
  EXPECT_EQ(2 * kCount, policy.destroy_object_count());
  EXPECT_EQ(8U, policy.init_block_count());
  EXPECT_EQ(4U, policy.destroy_block_count());

  cache.Clear();
  EXPECT_EQ(3 * kCount, policy.init_object_count());
  EXPECT_EQ(3 * kCount, policy.destroy_object_count());
  EXPECT_EQ(8U, policy.init_block_count());
  EXPECT_EQ(8U, policy.destroy_block_count());
}

// Test HashCache with |FramesUntilEviction| == 2, which means that objects are
// cached even if they are not used for an entire frame, but they are flushed
// from the cache if not used for two entire frames.  In this variant, either
// all objects are used in a frame, or none are.
TEST(HashCache, FullFrameToFrameCaching) {
  constexpr size_t kCount = 512;
  constexpr size_t kFramesUntilEviction = 2;
  HashCache<TestObj, TestObjObjectPoolPolicy, kFramesUntilEviction> cache;

  auto& policy = cache.object_pool().policy();
  EXPECT_EQ(0U, policy.init_block_count());
  EXPECT_EQ(0U, policy.destroy_block_count());

  // First frame: nothing is cached at first.
  cache.BeginFrame();
  ObtainAndValidateObjects(&cache, false, kCount);
  ObtainAndValidateObjects(&cache, true, kCount);

  // Everything is still cached next frame.
  cache.BeginFrame();
  ObtainAndValidateObjects(&cache, true, kCount);

  // Everything is still cached even if nothing is used for a frame.
  cache.BeginFrame();
  cache.BeginFrame();
  ObtainAndValidateObjects(&cache, true, kCount);

  // If an item isn't used for two frames, it is evicted from the cache.
  cache.BeginFrame();
  cache.BeginFrame();
  cache.BeginFrame();
  ObtainAndValidateObjects(&cache, false, kCount);

  // Double-check that everything is cached if nothing is used for a frame, and
  // evicted after two frames.
  cache.BeginFrame();
  cache.BeginFrame();
  ObtainAndValidateObjects(&cache, true, kCount);
  cache.BeginFrame();
  cache.BeginFrame();
  cache.BeginFrame();
  ObtainAndValidateObjects(&cache, false, kCount);

  // Cache's ObjectPool doesn't release underlying resources until the entire
  // cache is cleared.
  EXPECT_EQ(4U, policy.init_block_count());
  EXPECT_EQ(0U, policy.destroy_block_count());
  cache.Clear();
  EXPECT_EQ(4U, policy.init_block_count());
  EXPECT_EQ(4U, policy.destroy_block_count());
}

// Test HashCache with |FramesUntilEviction| == 2, which means that objects are
// cached even if they are not used for an entire frame, but they are flushed
// from the cache if not used for two entire frames.  In this variant, only some
// objects are used each frame.
TEST(HashCache, PartialFrameToFrameCaching) {
  constexpr size_t kCount = 512;
  constexpr size_t kFramesUntilEviction = 2;
  HashCache<TestObj, TestObjObjectPoolPolicy, kFramesUntilEviction> cache;

  auto& policy = cache.object_pool().policy();
  EXPECT_EQ(0U, policy.init_block_count());
  EXPECT_EQ(0U, policy.destroy_block_count());

  // First frame: nothing is cached at first.
  cache.BeginFrame();
  ObtainAndValidateObjects(&cache, false, kCount, 0);
  ObtainAndValidateObjects(&cache, true, kCount, 0);
  ObtainAndValidateObjects(&cache, false, kCount, kCount);
  ObtainAndValidateObjects(&cache, true, kCount, kCount);

  // Use half of the objects next frame.  They should still be cached.
  cache.BeginFrame();
  ObtainAndValidateObjects(&cache, true, kCount, 0);

  // Use the other half of the objects next frame.  They should still be cached.
  cache.BeginFrame();
  ObtainAndValidateObjects(&cache, true, kCount, kCount);

  // Skip a frame.  Only half of the objects should still be cached.
  cache.BeginFrame();
  cache.BeginFrame();
  ObtainAndValidateObjects(&cache, false, kCount, 0);
  ObtainAndValidateObjects(&cache, true, kCount, kCount);

  // Skip two frames.  No objects should still be cached.
  cache.BeginFrame();
  cache.BeginFrame();
  cache.BeginFrame();
  ObtainAndValidateObjects(&cache, false, 2 * kCount, 0);
}

TEST(HashCache, InFlightItemsRetainedUntilSequenceNumberFinishes) {
  constexpr uint32_t kFramesUntilEviction = 2;
  HashCache<TestObj, TestObjObjectPoolPolicy, kFramesUntilEviction> cache;
  auto& policy = cache.object_pool().policy();

  cache.BeginFrame(/*last_finished_sequence_number=*/0);
  auto [obj1, hit1] = cache.Obtain({1}, /*sequence_number=*/5);
  auto [obj2, hit2] = cache.Obtain({2}, /*sequence_number=*/8);
  EXPECT_FALSE(hit1);
  EXPECT_FALSE(hit2);
  EXPECT_EQ(5U, obj1->sequence_number());
  EXPECT_EQ(8U, obj2->sequence_number());

  // Advance past `kFramesUntilEviction` frames while sequence number 5 is still
  // in flight (`last_finished_sequence_number == 4`). Both items move to the
  // unretired ring rather than being freed to `object_pool_`.
  for (int i = 0; i < 5; ++i) {
    cache.BeginFrame(/*last_finished_sequence_number=*/4);
  }
  EXPECT_TRUE(cache.HasUnretiredItems());
  EXPECT_EQ(0U, policy.destroy_object_count());
  EXPECT_EQ(2U, cache.size());

  // Requesting `{1}` while it is in the unretired ring hits the cache, updates
  // its sequence number to 9, moves it back to the active frame ring, and
  // leaves `{2}` (sequence number 8) in the unretired ring.
  auto [obj1_reused, hit1_reused] = cache.Obtain({1}, /*sequence_number=*/9);
  EXPECT_TRUE(hit1_reused);
  EXPECT_EQ(obj1, obj1_reused);
  EXPECT_EQ(9U, obj1_reused->sequence_number());
  EXPECT_TRUE(cache.HasUnretiredItems());

  // Advance past `kFramesUntilEviction` frames with `last_finished == 8`, which
  // evicts `{2}` (seq 8) while retaining `{1}` (seq 9) in the unretired ring.
  for (int i = 0; i < 4; ++i) {
    cache.BeginFrame(/*last_finished_sequence_number=*/8);
  }
  EXPECT_TRUE(cache.HasUnretiredItems());
  EXPECT_EQ(1U, policy.destroy_object_count());
  EXPECT_EQ(1U, cache.size());

  // Once sequence number 9 finishes, the next `BeginFrame(9)` evicts `{1}` and
  // returns it to `object_pool_`.
  cache.BeginFrame(/*last_finished_sequence_number=*/9);
  EXPECT_FALSE(cache.HasUnretiredItems());
  EXPECT_EQ(2U, policy.destroy_object_count());
  EXPECT_EQ(0U, cache.size());
}

TEST(HashCache, NonMonotonicSequenceNumbersAndOutOfOrderUnretiredScan) {
  constexpr uint32_t kFramesUntilEviction = 2;
  HashCache<TestObj, TestObjObjectPoolPolicy, kFramesUntilEviction> cache;
  auto& policy = cache.object_pool().policy();

  cache.BeginFrame(/*last_finished_sequence_number=*/0);
  auto [obj1, hit1] = cache.Obtain({1}, /*sequence_number=*/8);
  EXPECT_FALSE(hit1);
  EXPECT_EQ(8U, obj1->sequence_number());

  // A hit with a lower sequence number must preserve the higher `sequence_number()`.
  auto [obj1_lower, hit1_lower] = cache.Obtain({1}, /*sequence_number=*/3);
  EXPECT_TRUE(hit1_lower);
  EXPECT_EQ(obj1, obj1_lower);
  EXPECT_EQ(8U, obj1->sequence_number());

  // Insert `{2}` (`seq == 5`) ahead of `{1}` (`seq == 8`) in the active frame
  // ring so `EvictFrameRing()` reverses their order to `[{1} (seq 8), {2} (seq 5)]`
  // in `rings_[kUnretiredRingIndex]`.
  auto [obj2, hit2] = cache.Obtain({2}, /*sequence_number=*/5);
  EXPECT_FALSE(hit2);
  EXPECT_EQ(5U, obj2->sequence_number());

  for (uint32_t f = 0; f <= kFramesUntilEviction; ++f) {
    cache.BeginFrame(/*last_finished_sequence_number=*/3);
  }
  EXPECT_TRUE(cache.HasUnretiredItems());
  EXPECT_EQ(0U, policy.destroy_object_count());
  EXPECT_EQ(2U, cache.size());

  // `BeginFrame(5)` must scan past retained `{1}` (`seq == 8`) at the front of
  // `rings_[kUnretiredRingIndex]` and evict `{2}` (`seq == 5`) behind it.
  cache.BeginFrame(/*last_finished_sequence_number=*/5);
  EXPECT_TRUE(cache.HasUnretiredItems());
  EXPECT_EQ(1U, policy.destroy_object_count());
  EXPECT_EQ(1U, cache.size());

  auto [obj2_reallocated, hit2_after_evict] = cache.Obtain({2}, /*sequence_number=*/5);
  EXPECT_FALSE(hit2_after_evict);
  EXPECT_NE(nullptr, obj2_reallocated);
  EXPECT_TRUE(cache.HasUnretiredItems());

  auto [obj1_retained, hit1_retained] = cache.Obtain({1}, /*sequence_number=*/8);
  EXPECT_TRUE(hit1_retained);
  EXPECT_EQ(obj1, obj1_retained);
  EXPECT_FALSE(cache.HasUnretiredItems());

  // Passing a lower `last_finished_sequence_number` (`0` after `5`) must not
  // regress `last_finished_sequence_number_`: `{2}` (`seq == 5`) is evicted as
  // soon as its frame window expires on `BeginFrame(0)`.
  for (uint32_t f = 0; f <= kFramesUntilEviction; ++f) {
    cache.BeginFrame(/*last_finished_sequence_number=*/0);
  }
  EXPECT_EQ(2U, policy.destroy_object_count());
  EXPECT_EQ(1U, cache.size());
  EXPECT_TRUE(cache.HasUnretiredItems());

  cache.BeginFrame(/*last_finished_sequence_number=*/8);
  EXPECT_FALSE(cache.HasUnretiredItems());
  EXPECT_EQ(0U, cache.size());
  EXPECT_EQ(policy.init_object_count(), policy.destroy_object_count());
}

TEST(HashCache, ClearEvictsUnretiredItemsAndResetsSequenceTracking) {
  constexpr uint32_t kFramesUntilEviction = 2;
  HashCache<TestObj, TestObjObjectPoolPolicy, kFramesUntilEviction> cache;
  auto& policy = cache.object_pool().policy();

  cache.BeginFrame(/*last_finished_sequence_number=*/0);
  auto [obj1, hit1] = cache.Obtain({1}, /*sequence_number=*/10);
  auto [obj2, hit2] = cache.Obtain({2}, /*sequence_number=*/10);
  EXPECT_FALSE(hit1);
  EXPECT_FALSE(hit2);
  EXPECT_NE(nullptr, obj1);
  EXPECT_NE(nullptr, obj2);

  for (uint32_t f = 0; f <= kFramesUntilEviction; ++f) {
    cache.BeginFrame(/*last_finished_sequence_number=*/0);
  }
  cache.BeginFrame(/*last_finished_sequence_number=*/5);
  EXPECT_TRUE(cache.HasUnretiredItems());
  EXPECT_EQ(0U, policy.destroy_object_count());

  cache.Clear();
  EXPECT_FALSE(cache.HasUnretiredItems());
  EXPECT_EQ(0U, cache.size());
  EXPECT_EQ(0U, cache.cache_hits());
  EXPECT_EQ(0U, cache.cache_misses());
  EXPECT_EQ(policy.init_object_count(), policy.destroy_object_count());
  EXPECT_EQ(policy.init_block_count(), policy.destroy_block_count());

  // Verify `Clear()` reset `last_finished_sequence_number_` and
  // `unretired_scan_sequence_number_` from `5` back to `0`.
  cache.BeginFrame(/*last_finished_sequence_number=*/0);
  auto [reused_obj, reused_hit] = cache.Obtain({1}, /*sequence_number=*/5);
  EXPECT_FALSE(reused_hit);
  EXPECT_NE(nullptr, reused_obj);
  for (uint32_t f = 0; f <= kFramesUntilEviction; ++f) {
    cache.BeginFrame(/*last_finished_sequence_number=*/0);
  }
  EXPECT_TRUE(cache.HasUnretiredItems());
  EXPECT_EQ(1U, cache.size());

  cache.BeginFrame(/*last_finished_sequence_number=*/5);
  EXPECT_FALSE(cache.HasUnretiredItems());
  EXPECT_EQ(0U, cache.size());
  EXPECT_EQ(policy.init_object_count(), policy.destroy_object_count());
}

TEST(HashCache, ReclaimCallbackEvictsCompletedUnretiredItemsOnPoolExhaustion) {
  constexpr uint32_t kFramesUntilEviction = 2;
  HashCache<TestObj, TestObjObjectPoolPolicy, kFramesUntilEviction> cache;
  auto& policy = cache.object_pool().policy();

  cache.BeginFrame(/*last_finished_sequence_number=*/0);
  const size_t initial_block_size = ObjectPool<TestObj>::InitialBlockSize();
  for (size_t i = 0; i < initial_block_size; ++i) {
    auto [obj, hit] = cache.Obtain({i + 1}, /*sequence_number=*/1);
    EXPECT_FALSE(hit);
  }
  EXPECT_EQ(1U, policy.init_block_count());
  EXPECT_EQ(0U, cache.object_pool().vacant_count());

  // Advance 3 frames with `last_finished_sequence_number == 0` so all 64 items
  // move into the unretired ring without being freed.
  cache.BeginFrame(/*last_finished_sequence_number=*/0);
  cache.BeginFrame(/*last_finished_sequence_number=*/0);
  cache.BeginFrame(/*last_finished_sequence_number=*/0);
  EXPECT_TRUE(cache.HasUnretiredItems());
  EXPECT_EQ(0U, cache.object_pool().vacant_count());
  EXPECT_EQ(0U, policy.destroy_object_count());

  // A cache miss when `vacant_count() == 0` and `HasUnretiredItems()` is true
  // invokes the exhaustion callback, which evicts completed unretired items
  // into `object_pool_` so the allocation reuses block 0 instead of allocating
  // a second block.
  size_t reclaim_calls = 0;
  auto [new_obj, hit] = cache.Obtain({9999}, /*sequence_number=*/2, [&] {
    ++reclaim_calls;
    EXPECT_EQ(initial_block_size, cache.EvictCompletedUnretiredItems(1));
  });
  EXPECT_FALSE(hit);
  ASSERT_NE(nullptr, new_obj);
  EXPECT_EQ(2U, new_obj->sequence_number());
  EXPECT_EQ(1U, reclaim_calls);
  EXPECT_EQ(1U, policy.init_block_count());
  EXPECT_EQ(initial_block_size, policy.destroy_object_count());
  EXPECT_EQ(initial_block_size - 1, cache.object_pool().vacant_count());

  // Move `new_obj` (`seq == 2`) into `rings_[kUnretiredRingIndex]` while 63
  // slots remain vacant: `vacant_count() > 0 && HasUnretiredItems()` must not
  // invoke `reclaim_on_exhaustion`.
  for (uint32_t f = 0; f <= kFramesUntilEviction; ++f) {
    cache.BeginFrame(/*last_finished_sequence_number=*/1);
  }
  ASSERT_GT(cache.object_pool().vacant_count(), 0U);
  ASSERT_TRUE(cache.HasUnretiredItems());
  auto [vacant_avail_obj, vacant_avail_hit] =
      cache.Obtain({10000}, /*sequence_number=*/3, [&] { ADD_FAILURE(); });
  EXPECT_FALSE(vacant_avail_hit);
  ASSERT_NE(nullptr, vacant_avail_obj);
  EXPECT_EQ(3U, vacant_avail_obj->sequence_number());

  // Retire `new_obj` and fill the remaining block-0 slots in active frame rings:
  // `vacant_count() == 0 && !HasUnretiredItems()` must grow `object_pool_` to
  // 2 blocks without invoking `reclaim_on_exhaustion`.
  cache.BeginFrame(/*last_finished_sequence_number=*/2);
  ASSERT_FALSE(cache.HasUnretiredItems());
  while (cache.object_pool().vacant_count() > 0) {
    const size_t next_key = 20000 + cache.object_pool().vacant_count();
    auto [fill_obj, fill_hit] = cache.Obtain({next_key}, /*sequence_number=*/3);
    EXPECT_FALSE(fill_hit);
    ASSERT_NE(nullptr, fill_obj);
  }
  ASSERT_EQ(0U, cache.object_pool().vacant_count());
  ASSERT_FALSE(cache.HasUnretiredItems());
  ASSERT_EQ(1U, cache.object_pool().num_blocks());

  auto [grown_obj, grown_hit] =
      cache.Obtain({30000}, /*sequence_number=*/3, [&] { ADD_FAILURE(); });
  EXPECT_FALSE(grown_hit);
  ASSERT_NE(nullptr, grown_obj);
  EXPECT_EQ(3U, grown_obj->sequence_number());
  EXPECT_EQ(2U, cache.object_pool().num_blocks());
}

TEST(HashCache, ReentrantObtainDuringReclaimCallbackReusesInsertedItem) {
  constexpr uint32_t kFramesUntilEviction = 2;
  HashCache<TestObj, TestObjObjectPoolPolicy, kFramesUntilEviction> cache;
  auto& policy = cache.object_pool().policy();

  cache.BeginFrame(/*last_finished_sequence_number=*/0);
  const size_t initial_block_size = ObjectPool<TestObj>::InitialBlockSize();
  for (size_t i = 0; i < initial_block_size; ++i) {
    auto [obj, hit] = cache.Obtain({i + 1}, /*sequence_number=*/1);
    EXPECT_FALSE(hit);
  }

  for (uint32_t f = 0; f <= kFramesUntilEviction; ++f) {
    cache.BeginFrame(/*last_finished_sequence_number=*/0);
  }
  ASSERT_TRUE(cache.HasUnretiredItems());
  ASSERT_EQ(0U, cache.object_pool().vacant_count());
  ASSERT_EQ(0U, cache.cache_hits());
  ASSERT_EQ(initial_block_size, cache.cache_misses());

  // When `reclaim_on_exhaustion` populates `same_hash` via a nested `Obtain()`
  // call and advances the active frame ring with `BeginFrame()`, the suspended
  // `Obtain()` call must reuse that cached item and move it to the new active
  // frame ring instead of allocating a duplicate object.
  constexpr Hash kSameHash = {9999};
  constexpr uint64_t kNestedSeq = 3;
  constexpr uint64_t kCallerSeq = 5;
  TestObj* callback_obj = nullptr;
  auto [obtained_obj, hit] = cache.Obtain(kSameHash, kCallerSeq, [&] {
    EXPECT_EQ(initial_block_size, cache.EvictCompletedUnretiredItems(1));
    auto [nested_obj, nested_hit] = cache.Obtain(kSameHash, kNestedSeq);
    EXPECT_FALSE(nested_hit);
    callback_obj = nested_obj;
    cache.BeginFrame(/*last_finished_sequence_number=*/1);
  });

  ASSERT_NE(nullptr, callback_obj);
  EXPECT_TRUE(hit);
  EXPECT_EQ(callback_obj, obtained_obj);
  EXPECT_EQ(std::max(kCallerSeq, kNestedSeq), obtained_obj->sequence_number());
  EXPECT_EQ(1U, cache.size());
  EXPECT_EQ(1U, cache.cache_hits());
  EXPECT_EQ(initial_block_size + 1, cache.cache_misses());
  EXPECT_EQ(1U, cache.object_pool().UnfreedObjectCount());

  const uint64_t max_seq = std::max(kCallerSeq, kNestedSeq);
  for (uint32_t f = 0; f < kFramesUntilEviction; ++f) {
    cache.BeginFrame(max_seq);
    EXPECT_EQ(1U, cache.size());
  }
  cache.BeginFrame(max_seq);
  EXPECT_EQ(0U, cache.size());
  EXPECT_FALSE(cache.HasUnretiredItems());
  EXPECT_EQ(0U, cache.object_pool().UnfreedObjectCount());
  EXPECT_EQ(policy.init_object_count(), policy.destroy_object_count());
}

}  // namespace
