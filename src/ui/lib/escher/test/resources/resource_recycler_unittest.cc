// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/lib/escher/resources/resource_recycler.h"

#include <atomic>
#include <thread>
#include <vector>

#include <gtest/gtest.h>

#include "src/lib/fxl/memory/ref_ptr.h"
#include "src/ui/lib/escher/resources/resource.h"
#include "src/ui/lib/escher/test/common/gtest_escher.h"

namespace escher {
namespace {

class TestResource : public Resource {
 public:
  static const ResourceTypeInfo kTypeInfo;
  const ResourceTypeInfo& type_info() const override { return kTypeInfo; }

  TestResource(ResourceManager* owner, uint64_t sequence_number,
               std::atomic<size_t>* destroyed_count, fxl::RefPtr<TestResource> child_a = nullptr,
               fxl::RefPtr<TestResource> child_b = nullptr)
      : Resource(owner),
        destroyed_count_(destroyed_count),
        child_a_(std::move(child_a)),
        child_b_(std::move(child_b)) {
    KeepAlive(sequence_number);
  }

  ~TestResource() override {
    if (destroyed_count_) {
      destroyed_count_->fetch_add(1, std::memory_order_relaxed);
    }
  }

 private:
  std::atomic<size_t>* const destroyed_count_;
  fxl::RefPtr<TestResource> child_a_;
  fxl::RefPtr<TestResource> child_b_;
};

const ResourceTypeInfo TestResource::kTypeInfo("TestResource", ResourceType::kResource);

VK_TEST(ResourceRecycler, ImmediateAndDeferredRecycleBySequenceNumber) {
  auto escher = test::GetEscher()->GetWeakPtr();
  ResourceRecycler recycler(escher);
  auto* listener = static_cast<impl::CommandBufferSequencerListener*>(&recycler);

  std::atomic<size_t> destroyed_count{0};

  // Sequence number 0 <= initial `last_finished_sequence_number_` (0): recycled immediately.
  auto resource_0 =
      fxl::MakeRefCounted<TestResource>(&recycler, /*sequence_number=*/0, &destroyed_count);
  resource_0 = nullptr;
  EXPECT_EQ(1u, destroyed_count.load());

  // Sequence numbers 2 and 5 > `last_finished_sequence_number_` (0): deferred in
  // `unused_resources_`.
  auto resource_2 =
      fxl::MakeRefCounted<TestResource>(&recycler, /*sequence_number=*/2, &destroyed_count);
  auto resource_5 =
      fxl::MakeRefCounted<TestResource>(&recycler, /*sequence_number=*/5, &destroyed_count);
  resource_2 = nullptr;
  resource_5 = nullptr;
  EXPECT_EQ(1u, destroyed_count.load());

  // Finishing sequence number 1 leaves both deferred.
  listener->OnCommandBufferFinished(1);
  EXPECT_EQ(1u, destroyed_count.load());

  // Finishing sequence number 2 recycles `resource_2` (sequence_number == 2) and keeps
  // `resource_5` deferred.
  listener->OnCommandBufferFinished(2);
  EXPECT_EQ(2u, destroyed_count.load());

  // Resources with sequence number == `last_finished_sequence_number_` (2) and
  // sequence number < `last_finished_sequence_number_` (1 < 2) both recycle immediately.
  auto equal_sequence_resource =
      fxl::MakeRefCounted<TestResource>(&recycler, /*sequence_number=*/2, &destroyed_count);
  equal_sequence_resource = nullptr;
  EXPECT_EQ(3u, destroyed_count.load());

  auto older_sequence_resource =
      fxl::MakeRefCounted<TestResource>(&recycler, /*sequence_number=*/1, &destroyed_count);
  older_sequence_resource = nullptr;
  EXPECT_EQ(4u, destroyed_count.load());

  // Finishing sequence number 6 recycles `resource_5` (sequence_number < 6).
  listener->OnCommandBufferFinished(6);
  EXPECT_EQ(5u, destroyed_count.load());
}

VK_TEST(ResourceRecycler, RecycleCompositeResourceReleasingChildResourcesDoesNotDeadlock) {
  auto escher = test::GetEscher()->GetWeakPtr();
  ResourceRecycler recycler(escher);
  auto* listener = static_cast<impl::CommandBufferSequencerListener*>(&recycler);

  std::atomic<size_t> destroyed_count{0};
  listener->OnCommandBufferFinished(1);

  // Case 1: Immediate recycle in `OnReceiveOwnable()` where destroying the parent drops two
  // children owned by the same `ResourceRecycler` (one recycled immediately, one deferred).
  {
    auto immediate_child =
        fxl::MakeRefCounted<TestResource>(&recycler, /*sequence_number=*/1, &destroyed_count);
    auto deferred_child =
        fxl::MakeRefCounted<TestResource>(&recycler, /*sequence_number=*/3, &destroyed_count);
    auto parent =
        fxl::MakeRefCounted<TestResource>(&recycler, /*sequence_number=*/1, &destroyed_count,
                                          std::move(immediate_child), std::move(deferred_child));

    // Dropping `parent` recycles `parent` and `immediate_child` immediately without deadlocking
    // `lock_`, while queuing `deferred_child` until sequence number 3 finishes.
    parent = nullptr;
    EXPECT_EQ(2u, destroyed_count.load());

    listener->OnCommandBufferFinished(3);
    EXPECT_EQ(3u, destroyed_count.load());
  }

  // Case 2: Deferred recycle in `OnCommandBufferFinished()` where destroying the parent drops
  // two children owned by the same `ResourceRecycler` (one recycled immediately, one deferred).
  {
    auto immediate_child =
        fxl::MakeRefCounted<TestResource>(&recycler, /*sequence_number=*/4, &destroyed_count);
    auto deferred_child =
        fxl::MakeRefCounted<TestResource>(&recycler, /*sequence_number=*/6, &destroyed_count);
    auto parent =
        fxl::MakeRefCounted<TestResource>(&recycler, /*sequence_number=*/4, &destroyed_count,
                                          std::move(immediate_child), std::move(deferred_child));

    // `parent` (sequence number 4) is greater than `last_finished_sequence_number_` (3, from
    // case 1), so dropping `parent` defers it until `OnCommandBufferFinished(4)`.
    parent = nullptr;
    EXPECT_EQ(3u, destroyed_count.load());

    listener->OnCommandBufferFinished(4);
    EXPECT_EQ(5u, destroyed_count.load());

    listener->OnCommandBufferFinished(6);
    EXPECT_EQ(6u, destroyed_count.load());
  }
}

VK_TEST(ResourceRecycler, ConcurrentOnReceiveOwnableAndOnCommandBufferFinishedNeverStrands) {
  auto escher = test::GetEscher()->GetWeakPtr();
  ResourceRecycler recycler(escher);
  auto* listener = static_cast<impl::CommandBufferSequencerListener*>(&recycler);

  constexpr size_t kNumThreads = 4;
  constexpr uint64_t kNumSteps = 250;
  constexpr size_t kTotalResources = kNumThreads * kNumSteps;

  std::atomic<size_t> destroyed_count{0};
  std::vector<std::vector<fxl::RefPtr<TestResource>>> per_thread_resources(kNumThreads);
  for (size_t t = 0; t < kNumThreads; ++t) {
    per_thread_resources[t].reserve(kNumSteps);
    for (uint64_t seq = 1; seq <= kNumSteps; ++seq) {
      per_thread_resources[t].push_back(
          fxl::MakeRefCounted<TestResource>(&recycler, seq, &destroyed_count));
    }
  }
  EXPECT_EQ(kTotalResources, recycler.ownable_count());

  std::atomic<size_t> ready_workers{0};
  std::atomic<uint64_t> current_step{0};
  std::atomic<size_t> step_completed_workers{0};
  std::vector<std::thread> workers;
  workers.reserve(kNumThreads);
  // At each step `seq`, publishing `current_step` lets every worker release its resource `seq`
  // concurrently with the main thread's `OnCommandBufferFinished(seq)`, so `OnReceiveOwnable()`
  // races the drain of `unused_resources_`. `ready_workers` ensures that all worker threads are
  // running and about to spin on `current_step` before step 1 is published. The race is detected
  // only probabilistically and requires multiple cores to overlap the two calls.
  for (size_t t = 0; t < kNumThreads; ++t) {
    workers.emplace_back([&, t]() {
      ready_workers.fetch_add(1, std::memory_order_release);
      for (uint64_t seq = 1; seq <= kNumSteps; ++seq) {
        while (current_step.load(std::memory_order_acquire) < seq) {
          std::this_thread::yield();
        }
        per_thread_resources[t][seq - 1] = nullptr;
        step_completed_workers.fetch_add(1, std::memory_order_release);
      }
    });
  }

  while (ready_workers.load(std::memory_order_acquire) < kNumThreads) {
    std::this_thread::yield();
  }
  for (uint64_t seq = 1; seq <= kNumSteps; ++seq) {
    step_completed_workers.store(0, std::memory_order_relaxed);
    current_step.store(seq, std::memory_order_release);
    listener->OnCommandBufferFinished(seq);
    while (step_completed_workers.load(std::memory_order_acquire) < kNumThreads) {
      std::this_thread::yield();
    }
    // Every resource up to `seq` must have been destroyed by the end of step `seq`, either
    // immediately in `OnReceiveOwnable()` or by `OnCommandBufferFinished(seq)`, without waiting
    // for a subsequent `OnCommandBufferFinished(seq + 1)` call to drain stranded entries.
    EXPECT_EQ(seq * kNumThreads, destroyed_count.load(std::memory_order_relaxed));
  }

  for (auto& worker : workers) {
    worker.join();
  }

  EXPECT_EQ(kTotalResources, destroyed_count.load(std::memory_order_relaxed));
  EXPECT_EQ(0u, recycler.ownable_count());
}

VK_TEST(ResourceRecycler, ChildReleasedOnWorkerWhileParentIsDrainedIsRecycledOnce) {
  auto escher = test::GetEscher()->GetWeakPtr();
  ResourceRecycler recycler(escher);
  auto* listener = static_cast<impl::CommandBufferSequencerListener*>(&recycler);

  constexpr uint64_t kNumIterations = 200;
  std::atomic<size_t> destroyed_count{0};

  for (uint64_t seq = 1; seq <= kNumIterations; ++seq) {
    auto child = fxl::MakeRefCounted<TestResource>(&recycler, seq, &destroyed_count);
    auto worker_child = child;
    auto parent =
        fxl::MakeRefCounted<TestResource>(&recycler, seq, &destroyed_count, std::move(child));

    // `parent` (sequence number `seq`) is deferred because only `seq - 1` has finished.
    parent = nullptr;
    EXPECT_EQ(2 * (seq - 1), destroyed_count.load(std::memory_order_relaxed));

    // The worker drops its reference to the child while the main thread's
    // `OnCommandBufferFinished(seq)` destroys `parent`, which drops the other reference. Whichever
    // thread drops the last reference calls `OnReceiveOwnable()` for the child exactly once, and
    // it must recycle the child immediately because `seq` has finished by then.
    std::atomic<bool> worker_ready{false};
    std::atomic<bool> start{false};
    std::thread worker([&]() {
      worker_ready.store(true, std::memory_order_release);
      while (!start.load(std::memory_order_acquire)) {
        std::this_thread::yield();
      }
      worker_child = nullptr;
    });
    while (!worker_ready.load(std::memory_order_acquire)) {
      std::this_thread::yield();
    }
    start.store(true, std::memory_order_release);
    listener->OnCommandBufferFinished(seq);
    worker.join();

    EXPECT_EQ(2 * seq, destroyed_count.load(std::memory_order_relaxed));
  }
  EXPECT_EQ(0u, recycler.ownable_count());
}

VK_TEST(ResourceRecycler, ReleasesRacingFinalCommandBufferFinishedAreNotStrandedAtTeardown) {
  auto escher = test::GetEscher()->GetWeakPtr();

  constexpr size_t kNumIterations = 100;
  constexpr size_t kNumThreads = 4;

  for (size_t iteration = 0; iteration < kNumIterations; ++iteration) {
    std::atomic<size_t> destroyed_count{0};
    {
      ResourceRecycler recycler(escher);
      auto* listener = static_cast<impl::CommandBufferSequencerListener*>(&recycler);

      std::vector<fxl::RefPtr<TestResource>> resources;
      resources.reserve(kNumThreads);
      for (size_t t = 0; t < kNumThreads; ++t) {
        resources.push_back(
            fxl::MakeRefCounted<TestResource>(&recycler, /*sequence_number=*/1, &destroyed_count));
      }

      // Workers release their resources concurrently with the final `OnCommandBufferFinished(1)`.
      // No later `OnCommandBufferFinished()` call drains `unused_resources_` before `recycler` is
      // destroyed, so any resource stranded by that race would fail the `destroyed_count`
      // expectation below and, in debug builds, trip `FX_DCHECK(unused_resources_.empty())` in
      // `~ResourceRecycler()`.
      std::atomic<size_t> ready_workers{0};
      std::atomic<bool> start{false};
      std::vector<std::thread> workers;
      workers.reserve(kNumThreads);
      for (size_t t = 0; t < kNumThreads; ++t) {
        workers.emplace_back([&, t]() {
          ready_workers.fetch_add(1, std::memory_order_release);
          while (!start.load(std::memory_order_acquire)) {
            std::this_thread::yield();
          }
          resources[t] = nullptr;
        });
      }
      while (ready_workers.load(std::memory_order_acquire) < kNumThreads) {
        std::this_thread::yield();
      }
      start.store(true, std::memory_order_release);
      listener->OnCommandBufferFinished(1);
      for (auto& worker : workers) {
        worker.join();
      }

      EXPECT_EQ(kNumThreads, destroyed_count.load(std::memory_order_relaxed));
      EXPECT_EQ(0u, recycler.ownable_count());
    }
  }
}

}  // namespace
}  // namespace escher
