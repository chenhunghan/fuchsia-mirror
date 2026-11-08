// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "allocation_table.h"

namespace memory_sampler {

// Marked `noinline` because this only runs on sampled allocations; keeping its
// overflow fallback out of `__scudo_allocate_hook` avoids stack-protector setup
// and extra register spills on the unsampled fast path.
[[gnu::noinline]] void AllocationTable::Insert(void* address) {
  if (address == nullptr) {
    return;
  }

  const auto addr = reinterpret_cast<uintptr_t>(address);
  Bucket& bucket = buckets_[GetBucketIndex(addr)];
  for (auto& way : bucket.ways) {
    uintptr_t current = way.load(std::memory_order_relaxed);
    if (current == 0 && way.compare_exchange_strong(current, addr, std::memory_order_release,
                                                    std::memory_order_relaxed)) {
      return;
    }
  }

  std::scoped_lock lock(lock_);
  if (overflow_allocations_.emplace(address).second) {
    overflow_count_.fetch_add(1, std::memory_order_release);
  }
}

bool AllocationTable::TryRemove(void* address) {
  if (address == nullptr) {
    return false;
  }

  const auto addr = reinterpret_cast<uintptr_t>(address);
  Bucket& bucket = buckets_[GetBucketIndex(addr)];
  for (auto& way : bucket.ways) {
    if (way.load(std::memory_order_relaxed) == addr) [[unlikely]] {
      uintptr_t expected = addr;
      if (way.compare_exchange_strong(expected, 0, std::memory_order_acq_rel,
                                      std::memory_order_relaxed)) {
        return true;
      }
    }
  }

  if (overflow_count_.load(std::memory_order_acquire) == 0) [[likely]] {
    return false;
  }

  std::scoped_lock lock(lock_);
  auto allocation = overflow_allocations_.find(address);
  if (allocation == overflow_allocations_.end()) {
    return false;
  }
  overflow_allocations_.erase(allocation);
  overflow_count_.fetch_sub(1, std::memory_order_release);
  return true;
}

void AllocationTable::Clear() {
  for (auto& bucket : buckets_) {
    for (auto& way : bucket.ways) {
      way.store(0, std::memory_order_relaxed);
    }
  }

  // Release any overflow tracked allocations outside of `lock_`: clearing
  // the set frees one node per entry, and this runs inside an allocator hook
  // on an arbitrary application thread. Swapping the set out under the lock is
  // O(1), and the nodes are then freed once `stale_allocations` goes out of
  // scope, without any other thread waiting on us.
  std::unordered_set<void*> stale_allocations;
  {
    std::scoped_lock lock(lock_);
    overflow_count_.store(0, std::memory_order_release);
    stale_allocations.swap(overflow_allocations_);
  }
}

}  // namespace memory_sampler
