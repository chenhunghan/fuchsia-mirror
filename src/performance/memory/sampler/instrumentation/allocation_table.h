// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_PERFORMANCE_MEMORY_SAMPLER_INSTRUMENTATION_ALLOCATION_TABLE_H_
#define SRC_PERFORMANCE_MEMORY_SAMPLER_INSTRUMENTATION_ALLOCATION_TABLE_H_

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <mutex>
#include <unordered_set>

namespace memory_sampler {

// Thread-safe set of live sampled allocation addresses.
//
// Most deallocations in a sampled process were never sampled on allocation.
// To avoid acquiring a global mutex on every unsampled `free()`, this table
// stores sampled addresses in a fixed-size lock-free 8-way set-associative
// table (64 buckets * 8 ways * 8B = 4 KiB), where each 8-way bucket fits in a
// single 64-byte cache line.
//
// If a bucket fills beyond 8 concurrent live sampled allocations, additional
// addresses spill into a mutex-guarded overflow set. An atomic counter tracks
// the number of overflow entries so unsampled deallocations that miss their
// bucket can skip acquiring the mutex whenever the overflow set is empty.
class AllocationTable {
 public:
  AllocationTable() = default;
  AllocationTable(const AllocationTable&) = delete;
  AllocationTable& operator=(const AllocationTable&) = delete;

  // Records `address` as a live sampled allocation. No-op if `address` is
  // `nullptr`. `address` must not already be present in the table (guaranteed
  // by Scudo invoking the deallocation hook before releasing a chunk for
  // reuse).
  void Insert(void* address);

  // Removes `address` if it is currently recorded and returns `true`; returns
  // `false` if `address` was not recorded.
  bool TryRemove(void* address);

  // Clears all recorded allocations from both the lock-free buckets and the
  // overflow set. Any heap nodes in the overflow set are freed outside the
  // internal lock.
  void Clear();

 private:
  friend struct AllocationTableTestPeer;

  static constexpr size_t kNumBuckets = 64;
  static constexpr size_t kWaysPerBucket = 8;
  static_assert((kNumBuckets & (kNumBuckets - 1)) == 0, "kNumBuckets must be a power of two");

  struct alignas(64) Bucket {
    std::atomic<uintptr_t> ways[kWaysPerBucket]{};
  };
  static_assert(sizeof(Bucket) == 64, "Bucket should fit in one cache line");

  static constexpr size_t GetBucketIndex(uintptr_t addr) {
    uint64_t x = addr;
    x ^= x >> 33u;
    x *= 0xff51afd7ed558ccdULL;
    x ^= x >> 33u;
    return static_cast<size_t>(x & (kNumBuckets - 1));
  }

  Bucket buckets_[kNumBuckets]{};

  // Fallback storage for addresses whose target bucket was full at insertion
  // time. `overflow_allocations_` is guarded by `lock_`, and `overflow_count_`
  // mirrors `overflow_allocations_.size()` (updated under `lock_` with release
  // ordering and read with acquire ordering) so `TryRemove` can skip acquiring
  // `lock_` whenever `overflow_allocations_` is empty.
  std::atomic<size_t> overflow_count_{0};
  std::mutex lock_;
  std::unordered_set<void*> overflow_allocations_;
};

}  // namespace memory_sampler

#endif  // SRC_PERFORMANCE_MEMORY_SAMPLER_INSTRUMENTATION_ALLOCATION_TABLE_H_
