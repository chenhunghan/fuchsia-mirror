// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/lib/escher/vk/impl/descriptor_set_allocator.h"

#include <lib/fit/defer.h>

#include <algorithm>
#include <vector>

#include "src/ui/lib/escher/defaults/default_shader_program_factory.h"
#include "src/ui/lib/escher/flatland/flatland_static_config.h"
#include "src/ui/lib/escher/flatland/rectangle_compositor.h"
#include "src/ui/lib/escher/impl/command_buffer.h"
#include "src/ui/lib/escher/impl/command_buffer_pool.h"
#include "src/ui/lib/escher/impl/command_buffer_sequencer.h"
#include "src/ui/lib/escher/renderer/batch_gpu_uploader.h"
#include "src/ui/lib/escher/renderer/frame.h"
#include "src/ui/lib/escher/renderer/render_funcs.h"
#include "src/ui/lib/escher/test/common/gtest_escher.h"
#include "src/ui/lib/escher/third_party/granite/vk/pipeline_layout.h"
#include "src/ui/lib/escher/util/image_utils.h"
#include "src/ui/lib/escher/vk/image_view.h"
#include "src/ui/lib/escher/vk/impl/framebuffer.h"
#include "src/ui/lib/escher/vk/shader_program.h"
#include "src/ui/lib/escher/vk/texture.h"

namespace {
using namespace escher;

class DescriptorSetAllocatorTest : public escher::test::TestWithVkValidationLayer {
 protected:
  static constexpr size_t kInitialBlockSize = 64;
  static constexpr size_t kFourBlockCapacity = 64 + 128 + 256 + 512;
  static constexpr size_t kFramesPastEvictionWindow = 5;

  void SetUp() override {
    TestWithVkValidationLayer::SetUp();
    Escher* escher = test::GetEscher();
    EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
    EXPECT_TRUE(escher->Cleanup());
  }

  void TearDown() override {
    Escher* escher = test::GetEscher();
    EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
    EXPECT_TRUE(escher->Cleanup());
    TestWithVkValidationLayer::TearDown();
  }

  static impl::DescriptorSetLayout CreateTestLayout() {
    impl::DescriptorSetLayout layout = {};
    layout.sampled_image_mask = 0x5;
    layout.storage_image_mask = 0x2;
    layout.input_attachment_mask = 0x8;
    return layout;
  }

  static std::vector<vk::DescriptorSet> AllocateDistinctSets(
      impl::DescriptorSetAllocator* allocator, size_t count, uint64_t sequence_number,
      size_t start_hash = 1) {
    std::vector<vk::DescriptorSet> sets;
    sets.reserve(count);
    for (size_t i = 0; i < count; ++i) {
      auto [set, hit] = allocator->Get({start_hash + i}, sequence_number);
      EXPECT_FALSE(hit);
      EXPECT_TRUE(set);
      sets.push_back(set);
    }
    return sets;
  }

  static void AdvancePastEvictionWindow(impl::DescriptorSetAllocator* allocator,
                                        uint64_t last_finished_sequence_number) {
    for (size_t i = 0; i < kFramesPastEvictionWindow; ++i) {
      allocator->BeginFrame(last_finished_sequence_number);
    }
  }

  static bool ContainsHandle(const std::vector<vk::DescriptorSet>& sets, vk::DescriptorSet target) {
    return std::find(sets.begin(), sets.end(), target) != sets.end();
  }
};

VK_TEST_F(DescriptorSetAllocatorTest, General) {
  Escher* escher = test::GetEscher();
  impl::DescriptorSetAllocator allocator(escher->vk_device(), CreateTestLayout(),
                                         escher->GetWeakPtr());

  Hash hash1 = {1};
  Hash hash2 = {2};

  allocator.BeginFrame(/*last_finished_sequence_number=*/0);
  std::pair<vk::DescriptorSet, bool> fr1_1 = allocator.Get(hash1, /*sequence_number=*/0);
  std::pair<vk::DescriptorSet, bool> fr1_1b = allocator.Get(hash1, /*sequence_number=*/0);
  std::pair<vk::DescriptorSet, bool> fr1_2 = allocator.Get(hash2, /*sequence_number=*/0);
  std::pair<vk::DescriptorSet, bool> fr1_2b = allocator.Get(hash2, /*sequence_number=*/0);

  // Only requests with the same hash value return the same descriptor set.
  EXPECT_EQ(fr1_1.first, fr1_1b.first);
  EXPECT_EQ(fr1_2.first, fr1_2b.first);
  EXPECT_NE(fr1_1.first, fr1_2.first);

  // The first request with a hash indicates that the descriptor set's contents
  // are invalid and must be written.  When a set is again requested for the
  // same hash, the allocator assumes that the previous caller wrote valid
  // descriptor values into the set.
  EXPECT_FALSE(fr1_1.second);
  EXPECT_TRUE(fr1_1b.second);
  EXPECT_FALSE(fr1_2.second);
  EXPECT_TRUE(fr1_2b.second);

  // Cached descriptor sets are available next frame.
  allocator.BeginFrame(/*last_finished_sequence_number=*/0);
  std::pair<vk::DescriptorSet, bool> fr2_1 = allocator.Get(hash1, /*sequence_number=*/0);
  EXPECT_EQ(fr2_1.first, fr1_1.first);
  EXPECT_TRUE(fr2_1.second);

  // They're also still available if not used for an entire frame, but then
  // requested the next frame.
  allocator.BeginFrame(/*last_finished_sequence_number=*/0);
  std::pair<vk::DescriptorSet, bool> fr3_2 = allocator.Get(hash2, /*sequence_number=*/0);
  EXPECT_EQ(fr3_2.first, fr1_2.first);
  EXPECT_TRUE(fr3_2.second);

  // However, if the hash isn't requested for four consecutive frames, then
  // requested again, then the resulting set's contents are invalid and must
  // be written.  In this case, there is no guarantee that the descriptor set
  // returned is the same one from 3 frames ago.
  allocator.BeginFrame(/*last_finished_sequence_number=*/0);
  allocator.BeginFrame(/*last_finished_sequence_number=*/0);
  allocator.BeginFrame(/*last_finished_sequence_number=*/0);
  allocator.BeginFrame(/*last_finished_sequence_number=*/0);
  std::pair<vk::DescriptorSet, bool> fr5_1 = allocator.Get(hash1, /*sequence_number=*/0);
  EXPECT_FALSE(fr5_1.second);

  // Of course, when re-requesting it in the same frame, it will be treated as
  // validly-cached.
  std::pair<vk::DescriptorSet, bool> fr5_1b = allocator.Get(hash1, /*sequence_number=*/0);
  EXPECT_EQ(fr5_1.first, fr5_1b.first);
  EXPECT_TRUE(fr5_1b.second);

  // During this test, there was a 62.5% cache hit rate.
  EXPECT_EQ(allocator.cache_hits(), 5U);
  EXPECT_EQ(allocator.cache_misses(), 3U);
}

VK_TEST_F(DescriptorSetAllocatorTest, InFlightDescriptorSetNotReusedBeforeCommandBufferFinishes) {
  Escher* escher = test::GetEscher();
  impl::DescriptorSetAllocator allocator(escher->vk_device(), CreateTestLayout(),
                                         escher->GetWeakPtr());

  Hash hash1 = {1};
  Hash hash2 = {2};
  allocator.BeginFrame(/*last_finished_sequence_number=*/0);
  auto [set1, hit1] = allocator.Get(hash1, /*sequence_number=*/10);
  EXPECT_FALSE(hit1);
  EXPECT_TRUE(set1);

  // Even after 6 frames without being requested, `set1` must not be evicted or
  // overwritten while sequence number 10 is still in flight.
  for (int i = 0; i < 6; ++i) {
    allocator.BeginFrame(/*last_finished_sequence_number=*/9);
  }

  // Requesting a distinct `hash2` while `set1` is unretired must allocate a
  // different `vk::DescriptorSet` rather than reusing `set1`.
  auto [set2, hit2] = allocator.Get(hash2, /*sequence_number=*/11);
  EXPECT_FALSE(hit2);
  EXPECT_TRUE(set2);
  EXPECT_NE(set1, set2);

  // Re-requesting `hash1` while its original command buffer is still in flight
  // hits the cache and returns the same `vk::DescriptorSet` without requiring
  // descriptor writes.
  auto [set1_in_flight, hit_in_flight] = allocator.Get(hash1, /*sequence_number=*/10);
  EXPECT_TRUE(hit_in_flight);
  EXPECT_EQ(set1, set1_in_flight);

  // Advance 5 more frames while sequence number 10 is still in flight, then
  // finish sequence number 10 on the 6th `BeginFrame()`.
  AdvancePastEvictionWindow(&allocator, /*last_finished_sequence_number=*/9);
  allocator.BeginFrame(/*last_finished_sequence_number=*/10);

  // Now `hash1` (`seq == 10`) has been evicted onto the top of
  // `ObjectPool::vacants_` while `set2` (`seq == 11`) remains retained, so the
  // next `Get()` re-initializes `set1`.
  auto [set1_after_finish, hit_after_finish] = allocator.Get(hash1, /*sequence_number=*/11);
  EXPECT_FALSE(hit_after_finish);
  EXPECT_TRUE(set1_after_finish);
  EXPECT_EQ(set1, set1_after_finish);
}

VK_TEST_F(DescriptorSetAllocatorTest, LazyCleanupReclaimsFinishedSetsWhenVacantPoolExhausted) {
  Escher* escher = test::GetEscher();
  impl::DescriptorSetAllocator allocator(escher->vk_device(), CreateTestLayout(),
                                         escher->GetWeakPtr());

  impl::CommandBuffer* cb = escher->command_buffer_pool()->GetCommandBuffer();
  const uint64_t seq = cb->sequence_number();

  allocator.BeginFrame(escher->command_buffer_sequencer()->last_finished_sequence_number());
  const std::vector<vk::DescriptorSet> initial_sets =
      AllocateDistinctSets(&allocator, kInitialBlockSize, seq);
  EXPECT_EQ(1U, allocator.num_pool_blocks());

  // Submit the command buffer and wait for the GPU to finish executing it, but
  // do NOT call `escher->Cleanup()` yet (simulating deferred cleanup).
  ASSERT_TRUE(cb->Submit(escher->device()->vk_main_queue(), nullptr));
  ASSERT_EQ(vk::Result::eSuccess, escher->vk_device().waitIdle());
  EXPECT_LT(escher->command_buffer_sequencer()->last_finished_sequence_number(), seq);

  // Advance 5 frames so all 64 sets expire their 4-frame window and move to the
  // unretired ring because `last_finished_sequence_number()` has not updated.
  AdvancePastEvictionWindow(&allocator,
                            escher->command_buffer_sequencer()->last_finished_sequence_number());
  EXPECT_EQ(1U, allocator.num_pool_blocks());

  // Requesting a 65th distinct hash when `vacant_count() == 0` triggers a lazy
  // `escher->CleanupCommandBufferPools()` that retires `cb`, reclaims the 64
  // expired sets, and satisfies the allocation from block 0 without allocating
  // a second block.
  auto [new_set, hit] = allocator.Get({kInitialBlockSize + 1}, seq + 1);
  EXPECT_FALSE(hit);
  EXPECT_TRUE(new_set);
  EXPECT_TRUE(ContainsHandle(initial_sets, new_set));
  EXPECT_GE(escher->command_buffer_sequencer()->last_finished_sequence_number(), seq);
  EXPECT_EQ(1U, allocator.num_pool_blocks());
}

VK_TEST_F(DescriptorSetAllocatorTest, UnsubmittedCommandBufferDoesNotDeadlockAtMaxPoolBlocks) {
  Escher* escher = test::GetEscher();
  impl::DescriptorSetAllocator allocator(escher->vk_device(), CreateTestLayout(),
                                         escher->GetWeakPtr());

  // Obtain a command buffer and keep it active (unsubmitted) while exhausting
  // all 4 initial blocks (64 + 128 + 256 + 512 = 960 sets).
  impl::CommandBuffer* cb = escher->command_buffer_pool()->GetCommandBuffer();
  auto submit_and_cleanup_cb = fit::defer([escher, cb] {
    EXPECT_TRUE(cb->Submit(escher->device()->vk_main_queue(), nullptr));
    EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
    EXPECT_TRUE(escher->Cleanup());
  });
  const uint64_t seq = cb->sequence_number();

  allocator.BeginFrame(escher->command_buffer_sequencer()->last_finished_sequence_number());
  const std::vector<vk::DescriptorSet> initial_sets =
      AllocateDistinctSets(&allocator, kFourBlockCapacity, seq);
  EXPECT_EQ(impl::DescriptorSetAllocator::kMaxPoolBlocksBeforeWait, allocator.num_pool_blocks());

  // Advance 5 frames while `cb` is still unsubmitted so all 960 sets move to
  // the unretired ring.
  AdvancePastEvictionWindow(&allocator,
                            escher->command_buffer_sequencer()->last_finished_sequence_number());

  // Requesting a 961st distinct hash hits `kMaxPoolBlocksBeforeWait`, sees that
  // the oldest pending command buffer is not yet submitted, and falls back to
  // allocating a 5th block instead of waiting on a fence that never signals.
  auto [extra_set, hit] = allocator.Get({kFourBlockCapacity + 1}, seq);
  EXPECT_FALSE(hit);
  EXPECT_TRUE(extra_set);
  EXPECT_FALSE(ContainsHandle(initial_sets, extra_set));
  EXPECT_EQ(impl::DescriptorSetAllocator::kMaxPoolBlocksBeforeWait + 1,
            allocator.num_pool_blocks());

  // Submit and clean up `cb` to leave `escher` in a clean state.
  submit_and_cleanup_cb.cancel();
  ASSERT_TRUE(cb->Submit(escher->device()->vk_main_queue(), nullptr));
  ASSERT_EQ(vk::Result::eSuccess, escher->vk_device().waitIdle());
  EXPECT_TRUE(escher->Cleanup());
  allocator.BeginFrame(escher->command_buffer_sequencer()->last_finished_sequence_number());
}

VK_TEST_F(DescriptorSetAllocatorTest, WaitsOnPendingCommandBufferAtMaxPoolBlocks) {
  Escher* escher = test::GetEscher();
  impl::DescriptorSetAllocator allocator(escher->vk_device(), CreateTestLayout(),
                                         escher->GetWeakPtr());
  vk::Queue main_queue = escher->device()->vk_main_queue();

  // Use `transfer_command_buffer_pool()` for `cb1` when available so its
  // callback, which submits `cb2`, runs after `command_buffer_pool()->Cleanup()`
  // in the initial `Escher::CleanupCommandBufferPools()` pass. `cb2` then stays
  // pending until the blocking wait loop in `ReclaimUnretiredSets()`.
  impl::CommandBufferPool* first_pool = escher->transfer_command_buffer_pool()
                                            ? escher->transfer_command_buffer_pool()
                                            : escher->command_buffer_pool();
  vk::Queue first_queue =
      escher->transfer_command_buffer_pool() ? escher->device()->vk_transfer_queue() : main_queue;

  impl::CommandBuffer* cb1 = first_pool->GetCommandBuffer();
  impl::CommandBuffer* cb2 = escher->command_buffer_pool()->GetCommandBuffer();
  impl::CommandBuffer* cb3 = escher->command_buffer_pool()->GetCommandBuffer();
  const uint64_t seq2 = cb2->sequence_number();
  const uint64_t seq3 = cb3->sequence_number();

  bool cb1_submitted = false;
  bool cb2_submitted = false;
  bool cb3_submitted = false;
  auto submit_and_drain = fit::defer([&] {
    if (!cb1_submitted) {
      EXPECT_TRUE(cb1->Submit(first_queue, nullptr));
    }
    if (!cb2_submitted) {
      EXPECT_TRUE(cb2->Submit(main_queue, nullptr));
    }
    if (!cb3_submitted) {
      EXPECT_TRUE(cb3->Submit(main_queue, nullptr));
    }
    EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
    EXPECT_TRUE(escher->Cleanup());
  });

  // Tag all 960 sets with `cb2`'s sequence number and age them into the
  // unretired ring before the 961st request misses.
  allocator.BeginFrame(escher->command_buffer_sequencer()->last_finished_sequence_number());
  const std::vector<vk::DescriptorSet> initial_sets =
      AllocateDistinctSets(&allocator, kFourBlockCapacity, seq2);
  EXPECT_EQ(impl::DescriptorSetAllocator::kMaxPoolBlocksBeforeWait, allocator.num_pool_blocks());
  AdvancePastEvictionWindow(&allocator,
                            escher->command_buffer_sequencer()->last_finished_sequence_number());

  cb1_submitted = true;
  ASSERT_TRUE(cb1->Submit(first_queue, [&] {
    cb2_submitted = true;
    EXPECT_TRUE(cb2->Submit(main_queue, nullptr));
  }));
  ASSERT_VK_SUCCESS(escher->vk_device().waitIdle());

  auto [reclaimed_set, hit] = allocator.Get({kFourBlockCapacity + 1}, seq3);
  EXPECT_FALSE(hit);
  EXPECT_TRUE(reclaimed_set);
  EXPECT_TRUE(ContainsHandle(initial_sets, reclaimed_set));
  EXPECT_TRUE(cb2_submitted);
  EXPECT_GE(escher->command_buffer_sequencer()->last_finished_sequence_number(), seq2);
  EXPECT_EQ(impl::DescriptorSetAllocator::kMaxPoolBlocksBeforeWait, allocator.num_pool_blocks());

  cb3_submitted = true;
  ASSERT_TRUE(cb3->Submit(main_queue, nullptr));
}

VK_TEST_F(DescriptorSetAllocatorTest, GrowsPoolWhenEscherWeakPtrIsNull) {
  Escher* escher = test::GetEscher();
  impl::DescriptorSetAllocator allocator(escher->vk_device(), CreateTestLayout(), EscherWeakPtr());

  allocator.BeginFrame(/*last_finished_sequence_number=*/0);
  const std::vector<vk::DescriptorSet> initial_sets =
      AllocateDistinctSets(&allocator, kInitialBlockSize, /*sequence_number=*/1);
  EXPECT_EQ(1U, allocator.num_pool_blocks());

  AdvancePastEvictionWindow(&allocator, /*last_finished_sequence_number=*/0);

  auto [new_set, hit] = allocator.Get({kInitialBlockSize + 1}, /*sequence_number=*/2);
  EXPECT_FALSE(hit);
  EXPECT_TRUE(new_set);
  EXPECT_FALSE(ContainsHandle(initial_sets, new_set));
  EXPECT_EQ(2U, allocator.num_pool_blocks());
}

VK_TEST_F(DescriptorSetAllocatorTest, ReclaimsAlreadyRetiredSetsWithoutPollingCommandBufferPools) {
  Escher* escher = test::GetEscher();
  impl::DescriptorSetAllocator allocator(escher->vk_device(), CreateTestLayout(),
                                         escher->GetWeakPtr());
  bool cb2_callback_ran = false;
  auto drain_callbacks = fit::defer([escher] {
    EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
    EXPECT_TRUE(escher->Cleanup());
  });

  impl::CommandBuffer* cb1 = escher->command_buffer_pool()->GetCommandBuffer();
  const uint64_t seq1 = cb1->sequence_number();
  const uint64_t pre_cleanup_finished =
      escher->command_buffer_sequencer()->last_finished_sequence_number();

  allocator.BeginFrame(pre_cleanup_finished);
  const std::vector<vk::DescriptorSet> initial_sets =
      AllocateDistinctSets(&allocator, kInitialBlockSize, seq1);
  ASSERT_TRUE(cb1->Submit(escher->device()->vk_main_queue(), nullptr));
  ASSERT_EQ(vk::Result::eSuccess, escher->vk_device().waitIdle());

  // Move all 64 sets into `unretired_ring` using the pre-cleanup sequence
  // number, then advance `last_finished_sequence_number()` via `Cleanup()`
  // without calling `allocator.BeginFrame()`.
  AdvancePastEvictionWindow(&allocator, pre_cleanup_finished);
  EXPECT_TRUE(escher->Cleanup());
  ASSERT_GE(escher->command_buffer_sequencer()->last_finished_sequence_number(), seq1);

  // Leave a signaled `cb2` uncleaned in `command_buffer_pool()` so its callback
  // detects whether `ReclaimUnretiredSets()` polled command buffer pools.
  impl::CommandBuffer* cb2 = escher->command_buffer_pool()->GetCommandBuffer();
  const uint64_t seq2 = cb2->sequence_number();
  ASSERT_TRUE(cb2->Submit(escher->device()->vk_main_queue(),
                          [&cb2_callback_ran] { cb2_callback_ran = true; }));
  ASSERT_EQ(vk::Result::eSuccess, escher->vk_device().waitIdle());

  auto [reclaimed_set, hit] = allocator.Get({kInitialBlockSize + 1}, seq2 + 1);
  EXPECT_FALSE(hit);
  EXPECT_TRUE(reclaimed_set);
  EXPECT_TRUE(ContainsHandle(initial_sets, reclaimed_set));
  EXPECT_EQ(1U, allocator.num_pool_blocks());
  EXPECT_FALSE(cb2_callback_ran);

  EXPECT_TRUE(escher->Cleanup());
  EXPECT_TRUE(cb2_callback_ran);
}

VK_TEST_F(DescriptorSetAllocatorTest, GrowsBelowMaxPoolBlocksInsteadOfWaitingOnInFlightSets) {
  Escher* escher = test::GetEscher();
  impl::DescriptorSetAllocator allocator(escher->vk_device(), CreateTestLayout(),
                                         escher->GetWeakPtr());

  impl::CommandBuffer* cb = escher->command_buffer_pool()->GetCommandBuffer();
  auto submit_and_cleanup_cb = fit::defer([escher, cb] {
    EXPECT_TRUE(cb->Submit(escher->device()->vk_main_queue(), nullptr));
    EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
    EXPECT_TRUE(escher->Cleanup());
  });
  const uint64_t seq = cb->sequence_number();

  allocator.BeginFrame(escher->command_buffer_sequencer()->last_finished_sequence_number());
  const std::vector<vk::DescriptorSet> initial_sets =
      AllocateDistinctSets(&allocator, kInitialBlockSize, seq);
  ASSERT_EQ(1U, allocator.num_pool_blocks());
  static_assert(1U < impl::DescriptorSetAllocator::kMaxPoolBlocksBeforeWait);

  AdvancePastEvictionWindow(&allocator,
                            escher->command_buffer_sequencer()->last_finished_sequence_number());

  auto [second_block_set, hit] = allocator.Get({kInitialBlockSize + 1}, seq);
  EXPECT_FALSE(hit);
  EXPECT_TRUE(second_block_set);
  EXPECT_FALSE(ContainsHandle(initial_sets, second_block_set));
  EXPECT_EQ(2U, allocator.num_pool_blocks());

  submit_and_cleanup_cb.cancel();
  ASSERT_TRUE(cb->Submit(escher->device()->vk_main_queue(), nullptr));
  ASSERT_EQ(vk::Result::eSuccess, escher->vk_device().waitIdle());
  EXPECT_TRUE(escher->Cleanup());
}

VK_TEST_F(DescriptorSetAllocatorTest, WaitsAcrossMultiplePendingCommandBuffersAtMaxPoolBlocks) {
  Escher* escher = test::GetEscher();
  impl::DescriptorSetAllocator allocator(escher->vk_device(), CreateTestLayout(),
                                         escher->GetWeakPtr());

  impl::CommandBufferPool* main_pool = escher->command_buffer_pool();
  vk::Queue main_queue = escher->device()->vk_main_queue();
  impl::CommandBufferPool* second_pool =
      escher->transfer_command_buffer_pool() ? escher->transfer_command_buffer_pool() : main_pool;
  vk::Queue second_queue =
      escher->transfer_command_buffer_pool() ? escher->device()->vk_transfer_queue() : main_queue;

  // Acquire four command buffers in strictly increasing sequence order across
  // `second_pool` (cleaned second in `CleanupCommandBufferPools()`) and
  // `main_pool` (cleaned first) so each retired buffer submits the next one
  // after `main_pool->Cleanup()` has already run in that pass, requiring
  // multiple iterations of the wait loop in `ReclaimUnretiredSets()` before
  // `seq3` finishes.
  impl::CommandBuffer* cb0 = second_pool->GetCommandBuffer();
  impl::CommandBuffer* cb1 = main_pool->GetCommandBuffer();
  impl::CommandBuffer* cb2 = second_pool->GetCommandBuffer();
  impl::CommandBuffer* cb3 = main_pool->GetCommandBuffer();
  const uint64_t seq3 = cb3->sequence_number();

  bool cb1_submitted = false;
  bool cb2_submitted = false;
  bool cb3_submitted = false;
  auto drain_pending = fit::defer([&] {
    if (!cb1_submitted) {
      EXPECT_TRUE(cb1->Submit(main_queue, nullptr));
    }
    if (!cb2_submitted) {
      EXPECT_TRUE(cb2->Submit(second_queue, nullptr));
    }
    if (!cb3_submitted) {
      EXPECT_TRUE(cb3->Submit(main_queue, nullptr));
    }
    EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
    EXPECT_TRUE(escher->Cleanup());
  });

  allocator.BeginFrame(escher->command_buffer_sequencer()->last_finished_sequence_number());
  const std::vector<vk::DescriptorSet> initial_sets =
      AllocateDistinctSets(&allocator, kFourBlockCapacity, seq3);
  EXPECT_EQ(impl::DescriptorSetAllocator::kMaxPoolBlocksBeforeWait, allocator.num_pool_blocks());
  AdvancePastEvictionWindow(&allocator,
                            escher->command_buffer_sequencer()->last_finished_sequence_number());

  ASSERT_TRUE(cb0->Submit(second_queue, [&] {
    cb1_submitted = true;
    EXPECT_TRUE(cb1->Submit(main_queue, [&] {
      cb2_submitted = true;
      EXPECT_TRUE(cb2->Submit(second_queue, [&] {
        cb3_submitted = true;
        EXPECT_TRUE(cb3->Submit(main_queue, nullptr));
      }));
    }));
  }));
  ASSERT_EQ(vk::Result::eSuccess, escher->vk_device().waitIdle());

  auto [reclaimed_set, hit] = allocator.Get({kFourBlockCapacity + 1}, seq3 + 1);
  EXPECT_FALSE(hit);
  EXPECT_TRUE(reclaimed_set);
  EXPECT_TRUE(ContainsHandle(initial_sets, reclaimed_set));
  EXPECT_GE(escher->command_buffer_sequencer()->last_finished_sequence_number(), seq3);
  EXPECT_EQ(impl::DescriptorSetAllocator::kMaxPoolBlocksBeforeWait, allocator.num_pool_blocks());
}

VK_TEST_F(DescriptorSetAllocatorTest, SkipsWaitDuringReentrantCleanupAtMaxPoolBlocks) {
  Escher* escher = test::GetEscher();
  impl::DescriptorSetAllocator allocator(escher->vk_device(), CreateTestLayout(),
                                         escher->GetWeakPtr());
  vk::DescriptorSet callback_set;
  bool callback_hit = true;
  bool callback_ran = false;
  auto drain_callbacks = fit::defer([escher] {
    EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
    EXPECT_TRUE(escher->Cleanup());
  });

  impl::CommandBuffer* cb1 = escher->command_buffer_pool()->GetCommandBuffer();
  impl::CommandBuffer* cb2 = escher->command_buffer_pool()->GetCommandBuffer();
  const uint64_t seq2 = cb2->sequence_number();
  bool cb1_submitted = false;
  bool cb2_submitted = false;
  auto submit_unsubmitted = fit::defer([&] {
    if (!cb1_submitted) {
      EXPECT_TRUE(cb1->Submit(escher->device()->vk_main_queue(), nullptr));
    }
    if (!cb2_submitted) {
      EXPECT_TRUE(cb2->Submit(escher->device()->vk_main_queue(), nullptr));
    }
  });

  allocator.BeginFrame(escher->command_buffer_sequencer()->last_finished_sequence_number());
  const std::vector<vk::DescriptorSet> initial_sets =
      AllocateDistinctSets(&allocator, kFourBlockCapacity, seq2);
  EXPECT_EQ(impl::DescriptorSetAllocator::kMaxPoolBlocksBeforeWait, allocator.num_pool_blocks());
  AdvancePastEvictionWindow(&allocator,
                            escher->command_buffer_sequencer()->last_finished_sequence_number());

  // Inside `cb1`'s callback, `command_buffer_pool()` is running `Cleanup()`
  // while `cb2`'s fence is already signaled, so a nested `Cleanup()` could not
  // retire `cb2`: `WaitOnOldestPendingCommandBuffer()` returns false without
  // waiting, and `ReclaimUnretiredSets()` falls back to allocating block 5.
  cb1_submitted = true;
  ASSERT_TRUE(cb1->Submit(escher->device()->vk_main_queue(), [&] {
    callback_ran = true;
    std::tie(callback_set, callback_hit) = allocator.Get({kFourBlockCapacity + 1}, seq2);
  }));
  cb2_submitted = true;
  ASSERT_TRUE(cb2->Submit(escher->device()->vk_main_queue(), nullptr));

  ASSERT_EQ(vk::Result::eSuccess, escher->vk_device().waitIdle());
  EXPECT_TRUE(escher->Cleanup());

  EXPECT_TRUE(callback_ran);
  EXPECT_FALSE(callback_hit);
  EXPECT_TRUE(callback_set);
  EXPECT_FALSE(ContainsHandle(initial_sets, callback_set));
  EXPECT_EQ(impl::DescriptorSetAllocator::kMaxPoolBlocksBeforeWait + 1,
            allocator.num_pool_blocks());
  EXPECT_GE(escher->command_buffer_sequencer()->last_finished_sequence_number(), seq2);
}

VK_TEST_F(DescriptorSetAllocatorTest, EndToEndFrameAndCommandBufferFlushRetainInFlightSet) {
  Escher* escher = test::GetEscher();
  auto cleanup_on_exit = fit::defer([escher] {
    EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
    EXPECT_TRUE(escher->Cleanup());
  });

  uint64_t frame_number = 0;
  TexturePtr tex1;
  TexturePtr tex2;
  ImagePtr color_attachment =
      image_utils::NewImage(escher->image_cache(), vk::Format::eB8G8R8A8Srgb, ColorSpace::kSrgb, 16,
                            16, vk::ImageUsageFlagBits::eColorAttachment);
  ASSERT_TRUE(color_attachment);
  color_attachment->set_swapchain_layout(vk::ImageLayout::eColorAttachmentOptimal);
  {
    auto uploader = BatchGpuUploader::New(escher->GetWeakPtr(), ++frame_number);
    const uint8_t white[4] = {255, 255, 255, 255};
    const uint8_t red[4] = {255, 0, 0, 255};
    tex1 =
        escher->NewTexture(escher->NewRgbaImage(uploader.get(), 1, 1, white), vk::Filter::eNearest);
    tex2 =
        escher->NewTexture(escher->NewRgbaImage(uploader.get(), 1, 1, red), vk::Filter::eNearest);
    uploader->Submit();

    auto init_frame = escher->NewFrame("init_color_attachment", ++frame_number);
    init_frame->cmds()->ImageBarrier(color_attachment, vk::ImageLayout::eUndefined,
                                     vk::ImageLayout::eColorAttachmentOptimal,
                                     vk::PipelineStageFlagBits::eTopOfPipe, vk::AccessFlags(),
                                     vk::PipelineStageFlagBits::eColorAttachmentOutput,
                                     vk::AccessFlagBits::eColorAttachmentWrite);
    init_frame->EndFrame(SemaphorePtr(), nullptr);
    ASSERT_EQ(vk::Result::eSuccess, escher->vk_device().waitIdle());
    EXPECT_TRUE(escher->Cleanup());
  }

  auto depth_format_result = escher->device()->caps().GetMatchingDepthStencilFormat();
  ASSERT_EQ(vk::Result::eSuccess, depth_format_result.result);
  TexturePtr depth_texture;
  RenderFuncs::ObtainDepthTexture(escher, /*use_protected_memory=*/false, color_attachment->info(),
                                  depth_format_result.value, depth_texture);
  ASSERT_TRUE(depth_texture);

  RectangleCompositor compositor(escher->GetWeakPtr());
  impl::FramebufferPtr framebuffer =
      compositor.CreateFramebuffer(ImageView::New(color_attachment), depth_texture);
  ASSERT_TRUE(framebuffer);

  ShaderProgramPtr program = escher->shader_program_factory()->GetProgram(kFlatlandStandardProgram);
  ASSERT_TRUE(program);
  PipelineLayoutPtr pipeline_layout =
      program->ObtainPipelineLayout(escher->pipeline_layout_cache(), /*immutable_sampler=*/nullptr);
  ASSERT_TRUE(pipeline_layout);
  impl::DescriptorSetAllocator* allocator = pipeline_layout->GetDescriptorSetAllocator(0);
  ASSERT_NE(nullptr, allocator);

  const Rectangle2D rect(vec2(0, 0), vec2(8, 8));
  const RectangleCompositor::ColorData color_data(vec4(1), RectangleCompositor::Opacity::Opaque);

  // Flush `tex1` on an unretired graphics `Frame` (`skip_escher_cleanup == true`).
  const size_t misses_before_tex1 = allocator->cache_misses();
  auto frame1 = escher->NewFrame("flush_tex1", ++frame_number);
  const uint64_t frame1_seq = frame1->cmds()->sequence_number();
  compositor.DrawBatch(frame1->cmds(), {rect}, {tex1}, {color_data}, framebuffer);
  frame1->EndFrame(SemaphorePtr(), nullptr, /*skip_escher_cleanup=*/true);
  EXPECT_EQ(misses_before_tex1 + 1, allocator->cache_misses());
  EXPECT_LT(escher->command_buffer_sequencer()->last_finished_sequence_number(), frame1_seq);

  // Advance past `FramesUntilEviction` graphics frames without cleaning up
  // `frame1`, moving `tex1`'s descriptor set into `rings_[kUnretiredRingIndex]`.
  for (size_t i = 0; i < kFramesPastEvictionWindow; ++i) {
    auto idle_frame = escher->NewFrame("idle_frame", ++frame_number);
    idle_frame->EndFrame(SemaphorePtr(), nullptr, /*skip_escher_cleanup=*/true);
  }
  EXPECT_LT(escher->command_buffer_sequencer()->last_finished_sequence_number(), frame1_seq);

  // Flushing `tex2` on the same `PipelineLayout` must allocate a separate set
  // without overwriting `tex1`'s in-flight descriptor set.
  const size_t misses_before_tex2 = allocator->cache_misses();
  auto frame_tex2 = escher->NewFrame("flush_tex2", ++frame_number);
  compositor.DrawBatch(frame_tex2->cmds(), {rect}, {tex2}, {color_data}, framebuffer);
  frame_tex2->EndFrame(SemaphorePtr(), nullptr, /*skip_escher_cleanup=*/true);
  EXPECT_EQ(misses_before_tex2 + 1, allocator->cache_misses());
  EXPECT_NO_VULKAN_VALIDATION_ERRORS();

  // Re-binding `tex1` before `Cleanup()` hits its retained unretired entry,
  // which moves `tex1` back to the active frame ring; age it back into
  // `rings_[kUnretiredRingIndex]` afterward.
  const size_t hits_before_rebind = allocator->cache_hits();
  const size_t misses_before_rebind = allocator->cache_misses();
  auto frame_rebind = escher->NewFrame("rebind_tex1", ++frame_number);
  const uint64_t rebind_seq = frame_rebind->cmds()->sequence_number();
  compositor.DrawBatch(frame_rebind->cmds(), {rect}, {tex1}, {color_data}, framebuffer);
  frame_rebind->EndFrame(SemaphorePtr(), nullptr, /*skip_escher_cleanup=*/true);
  EXPECT_EQ(hits_before_rebind + 1, allocator->cache_hits());
  EXPECT_EQ(misses_before_rebind, allocator->cache_misses());

  for (size_t i = 0; i < kFramesPastEvictionWindow; ++i) {
    auto age_frame = escher->NewFrame("age_tex1", ++frame_number);
    age_frame->EndFrame(SemaphorePtr(), nullptr, /*skip_escher_cleanup=*/true);
  }
  EXPECT_LT(escher->command_buffer_sequencer()->last_finished_sequence_number(), rebind_seq);

  // Once `Cleanup()` advances `last_finished_sequence_number()`, the next
  // `Escher::NewFrame()` evicts `tex1`'s unretired set so binding `tex1` again
  // misses the cache.
  ASSERT_EQ(vk::Result::eSuccess, escher->vk_device().waitIdle());
  EXPECT_TRUE(escher->Cleanup());
  ASSERT_GE(escher->command_buffer_sequencer()->last_finished_sequence_number(), rebind_seq);

  const size_t misses_after_cleanup = allocator->cache_misses();
  auto final_frame = escher->NewFrame("post_cleanup_tex1", ++frame_number);
  compositor.DrawBatch(final_frame->cmds(), {rect}, {tex1}, {color_data}, framebuffer);
  final_frame->EndFrame(SemaphorePtr(), nullptr);
  EXPECT_EQ(misses_after_cleanup + 1, allocator->cache_misses());

  ASSERT_EQ(vk::Result::eSuccess, escher->vk_device().waitIdle());
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_NO_VULKAN_VALIDATION_ERRORS();
}

}  // namespace
