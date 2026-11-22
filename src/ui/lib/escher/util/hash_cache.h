// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_LIB_ESCHER_UTIL_HASH_CACHE_H_
#define SRC_UI_LIB_ESCHER_UTIL_HASH_CACHE_H_

#include <algorithm>
#include <cstdint>
#include <limits>

#include "src/ui/lib/escher/util/hash.h"
#include "src/ui/lib/escher/util/hash_map.h"
#include "src/ui/lib/escher/util/intrusive_list.h"
#include "src/ui/lib/escher/util/object_pool.h"

#ifndef NDEBUG
#include <lib/syslog/cpp/macros.h>
#endif

namespace escher {

// Base class for items that can be cached in a HashCache.
//
// TODO(https://fxbug.dev/42098056): A fancier implementation would make these fields private.
// Until then, we trust Escher clients to not frob them.
template <typename T>
struct HashCacheItem : public IntrusiveListItem<T> {
 public:
  void set_hash(Hash hash) { hash_ = hash; }
  void set_ring_index(size_t ring_index) { ring_index_ = ring_index; }
  void set_sequence_number(uint64_t sequence_number) { sequence_number_ = sequence_number; }
  void UpdateSequenceNumber(uint64_t sequence_number) {
    sequence_number_ = std::max(sequence_number_, sequence_number);
  }
  Hash hash() { return hash_; }
  size_t ring_index() const { return ring_index_; }
  uint64_t sequence_number() const { return sequence_number_; }

 private:
  // The hash is stored so that the item can be found in the map when it is
  // found in the ring-to-be-evicted.
  Hash hash_ = {0};

  // Index in `HashCache::rings_` that currently owns this item (either its
  // last-used frame ring or `HashCache::kUnretiredRingIndex`). When used in a
  // different frame, the item moves to that frame's ring.
  size_t ring_index_ = 0;

  // Highest command buffer sequence number that has referenced this item.
  // Frame-ring and unretired-ring eviction do not free an item back to
  // `object_pool_` until its `sequence_number_` is at most
  // `last_finished_sequence_number_` (`HashCache::Clear()` frees it regardless).
  uint64_t sequence_number_ = 0;
};

// Wraps the provided ObjectPool policy so that HashCache clients can focus on
// the domain-specific functionality of cached items, without thinking about
// the HashCache-internal fields of those items.
template <typename T, typename BasePolicyT>
class HashCacheObjectPoolPolicy : public BasePolicyT {
 public:
  template <typename... Args>
  HashCacheObjectPoolPolicy(Args&&... args) : BasePolicyT(std::forward<Args>(args)...) {}

  inline void DestroyPoolObject(T* ptr) {
#ifndef NDEBUG
    FX_DCHECK(ptr->prev == nullptr);
    FX_DCHECK(ptr->next == nullptr);
    FX_DCHECK(ptr->list == nullptr);
#endif
    ptr->set_hash({0});
    ptr->set_ring_index(std::numeric_limits<size_t>::max());
    ptr->set_sequence_number(0);
    this->BasePolicyT::DestroyPoolObject(ptr);
  }
};

// Provides a frame-based cache that evicts an item once both
// `FramesUntilEviction` frames have passed without use and its referencing
// `sequence_number()` has finished:
// - if `FramesUntilEviction == 0`, finished items are evicted on the next
//   `BeginFrame()` even if they were used in the previous frame.
// - if `FramesUntilEviction == 1`, finished items are evicted if they were not
//   used in the previous frame, otherwise they remain in the cache.
// - if `FramesUntilEviction == 2`, finished items are evicted if two frames
//   pass without the item being used.
// - and so on...
// In-flight items (`sequence_number() > last_finished_sequence_number_`) remain
// cached in the unretired ring across frames even when
// `FramesUntilEviction == 0`.
//
// Clients are responsible for not using the cached object after it has been
// evicted from the cache.  The recommended way to do this is to re-request
// cached objects every frame (or even more often).
template <typename T, typename ObjectPoolPolicyT = DefaultObjectPoolPolicy<T>,
          uint32_t FramesUntilEviction = 4>
class HashCache {
 public:
  template <typename... Args>
  HashCache(Args&&... args) : object_pool_(std::forward<Args>(args)...) {}
  ~HashCache() { Clear(); }

  // Immediately frees every item, including unretired items still referenced by
  // in-flight command buffers, so callers must ensure that GPU work referencing
  // cached items has completed first.
  void Clear() {
#ifndef NDEBUG
    FX_DCHECK(reclaim_depth_ == 0) << "Clear() called from Obtain()'s reclaim_on_exhaustion";
#endif
    for (size_t i = 0; i < kTotalRingCount; ++i) {
      ForceClearRing(i);
    }
    hash_map_.clear();

    object_pool_.Clear();

    last_finished_sequence_number_ = 0;
    unretired_scan_sequence_number_ = 0;
    cache_hits_ = 0;
    cache_misses_ = 0;
  }

  // Non-zero `last_finished_sequence_number` values passed to `BeginFrame()` and
  // `EvictCompletedUnretiredItems()` must come from one monotonically
  // non-decreasing timeline until `Clear()`; smaller values are ignored.
  void BeginFrame(uint64_t last_finished_sequence_number = 0) {
    EvictCompletedUnretiredItems(last_finished_sequence_number);
    index_ = (index_ + 1) % kFrameRingCount;
    EvictFrameRing(index_);
  }

  // Evict any items in `rings_[kUnretiredRingIndex]` whose command buffer
  // sequence number is at most `last_finished_sequence_number`. Returns the
  // number of items freed back to `object_pool_`.
  size_t EvictCompletedUnretiredItems(uint64_t last_finished_sequence_number) {
    last_finished_sequence_number_ =
        std::max(last_finished_sequence_number_, last_finished_sequence_number);
    auto& unretired_ring = rings_[kUnretiredRingIndex];
    if (unretired_ring.IsEmpty()) {
      // Prevent a redundant scan once `EvictFrameRing()` later adds items above this watermark.
      unretired_scan_sequence_number_ = last_finished_sequence_number_;
      return 0;
    }
    // Items only enter `unretired_ring` with `sequence_number()` greater than
    // the `last_finished_sequence_number_` at that time, so if it has not
    // advanced since the previous scan, nothing in the ring can be evicted.
    if (last_finished_sequence_number_ == unretired_scan_sequence_number_) {
      return 0;
    }
    unretired_scan_sequence_number_ = last_finished_sequence_number_;

    size_t evicted_count = 0;
    auto it = unretired_ring.begin();
    while (it != unretired_ring.end()) {
      T* item = it.get();
      if (item->sequence_number() <= last_finished_sequence_number_) {
        it = unretired_ring.Erase(it);
        hash_map_.erase(item->hash());
        object_pool_.Free(item);
        ++evicted_count;
      } else {
        ++it;
      }
    }
    return evicted_count;
  }

  bool HasUnretiredItems() const { return !rings_[kUnretiredRingIndex].IsEmpty(); }

  // Look up `hash` and tag the item with `sequence_number`, allocating a new
  // item if needed. Returns `{item, was_cached}` where `was_cached` is true on
  // a cache hit and false when newly allocated; `item` remains valid until
  // evicted or cleared. When a miss finds the pool exhausted with unretired
  // items, `reclaim_on_exhaustion` runs before allocation and may call
  // `EvictCompletedUnretiredItems()`, `BeginFrame()`, or `Obtain()` (including
  // for `hash`), but must not call `Clear()` or destroy this `HashCache` (both
  // are checked with `FX_DCHECK` in debug builds).
  template <typename ReclaimFunc>
  std::pair<T*, bool> Obtain(Hash hash, uint64_t sequence_number,
                             ReclaimFunc&& reclaim_on_exhaustion) {
    auto it = hash_map_.find(hash);
    if (it == hash_map_.end() && object_pool_.vacant_count() == 0 && HasUnretiredItems()) {
      // `reclaim_on_exhaustion()` may call `Obtain()` (including for `hash`) or
      // `BeginFrame()`, so look up `hash` again before allocating a new item.
#ifndef NDEBUG
      ++reclaim_depth_;
#endif
      reclaim_on_exhaustion();
#ifndef NDEBUG
      --reclaim_depth_;
#endif
      it = hash_map_.find(hash);
    }
    if (it != hash_map_.end()) {
      // Item was already cached.
      ++cache_hits_;

      T* item = it->second;
      FX_DCHECK(item->hash() == hash);
      item->UpdateSequenceNumber(sequence_number);

      // Move item to the current frame's ring, to prevent it from being flushed
      // from the cache.
      if (item->ring_index() != index_) {
        rings_[index_].MoveToFront(rings_[item->ring_index()], item);
        item->set_ring_index(index_);
      }

      return {item, true};
    }

    // The item was not found in the cache, so a new one must be allocated.
    ++cache_misses_;

    T* item = object_pool_.Allocate();
    item->set_hash(hash);
    item->set_ring_index(index_);
    item->set_sequence_number(sequence_number);

    [[maybe_unused]] const bool inserted = hash_map_.emplace(hash, item).second;
    FX_DCHECK(inserted);
    rings_[index_].InsertFront(item);

    return {item, false};
  }

  std::pair<T*, bool> Obtain(Hash hash, uint64_t sequence_number = 0) {
    return Obtain(hash, sequence_number, [] {});
  }

  size_t cache_hits() const { return cache_hits_; }
  size_t cache_misses() const { return cache_misses_; }
  size_t size() const { return hash_map_.size(); }

  using ObjectPoolPolicyType = HashCacheObjectPoolPolicy<T, ObjectPoolPolicyT>;
  using ObjectPoolType = ObjectPool<T, ObjectPoolPolicyType>;
  const ObjectPoolType& object_pool() const { return object_pool_; }

 private:
  static constexpr size_t kFrameRingCount = FramesUntilEviction + 1;
  // Holds items that have gone `FramesUntilEviction` frames without CPU use
  // while their referencing command buffer is still in flight on the GPU
  // (`sequence_number() > last_finished_sequence_number_`). Keeping these items
  // in `hash_map_` allows subsequent requests for the same `Hash` to hit the
  // cache without allocating duplicate pool objects while preventing premature
  // reuse via `object_pool_.Free()`.
  static constexpr size_t kUnretiredRingIndex = kFrameRingCount;
  static constexpr size_t kTotalRingCount = kFrameRingCount + 1;

  void EvictFrameRing(size_t ring_index) {
    FX_DCHECK(ring_index < kFrameRingCount);
    auto& ring = rings_[ring_index];
    while (auto item = ring.PopFront()) {
      if (item->sequence_number() > last_finished_sequence_number_) {
        item->set_ring_index(kUnretiredRingIndex);
        rings_[kUnretiredRingIndex].InsertFront(item);
        continue;
      }
      hash_map_.erase(item->hash());
      object_pool_.Free(item);
    }
    FX_DCHECK(ring.IsEmpty());
  }

  void ForceClearRing(size_t ring_index) {
    FX_DCHECK(ring_index < kTotalRingCount);
    auto& ring = rings_[ring_index];
    while (auto item = ring.PopFront()) {
      hash_map_.erase(item->hash());
      object_pool_.Free(item);
    }
    FX_DCHECK(ring.IsEmpty());
  }

  IntrusiveList<T> rings_[kTotalRingCount];
  ObjectPoolType object_pool_;
  size_t index_ = 0;
  uint64_t last_finished_sequence_number_ = 0;
  // `last_finished_sequence_number_` up to which `rings_[kUnretiredRingIndex]`
  // is known to contain no completed items (updated on each scan or empty-ring
  // check in `EvictCompletedUnretiredItems()`).
  uint64_t unretired_scan_sequence_number_ = 0;
#ifndef NDEBUG
  // Number of `reclaim_on_exhaustion()` calls in progress inside `Obtain()`.
  // `Clear()` must not run while it is nonzero, because the suspended
  // `Obtain()` call would then insert into a cleared cache.
  uint32_t reclaim_depth_ = 0;
#endif

  HashMap<Hash, T*> hash_map_;
  size_t cache_hits_ = 0;
  size_t cache_misses_ = 0;
};

}  // namespace escher

#endif  // SRC_UI_LIB_ESCHER_UTIL_HASH_CACHE_H_
