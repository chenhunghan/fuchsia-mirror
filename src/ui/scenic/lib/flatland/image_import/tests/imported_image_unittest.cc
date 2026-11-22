// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Design: ../DESIGN.md. A change that contradicts a rule there amends the rule
// in the same CL, with the reason (../DESIGN.md#changing).

#include "src/ui/scenic/lib/flatland/image_import/imported_image.h"

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fit/defer.h>
#include <lib/zx/time.h>

#include <cstdint>
#include <memory>
#include <memory_resource>
#include <string_view>
#include <vector>

#include <gtest/gtest.h>

#include "src/ui/scenic/lib/allocation/id.h"
#include "src/ui/scenic/lib/flatland/image_import/image_importer.h"
#include "src/ui/scenic/lib/flatland/image_import/imported_collection.h"
#include "src/ui/scenic/lib/flatland/image_import/testing/image_import_test_peer.h"
#include "src/ui/scenic/lib/utils/check_is_on_thread.h"
#include "src/ui/scenic/lib/utils/pmr_stats_resource.h"

namespace flatland {
namespace {

class DummyGpuImage final : public ImportedImage::GpuImage {
 public:
  explicit DummyGpuImage(bool* destroyed = nullptr) : destroyed_(destroyed) {}
  ~DummyGpuImage() override {
    if (destroyed_) {
      *destroyed_ = true;
    }
  }

 private:
  bool* destroyed_;
};

class DummyGpuCollection final : public ImportedCollection::GpuCollection {};

allocation::ImageMetadata MakeTestMetadata(uint64_t collection_id = 10, uint64_t image_id = 20) {
  return allocation::ImageMetadata{
      .collection_id = collection_id,
      .identifier = allocation::GlobalImageId(image_id),
      .vmo_index = 0,
      .width = 64,
      .height = 32,
  };
}

TEST(ImportImageErrorTest, ErrorStringsCoverAllValuesAndOutOfRangeFallback) {
  struct Case {
    ImportImageError error;
    std::string_view expected;
  };
  constexpr Case kCases[] = {
      {ImportImageError::kUnknownCollection, "unknown_collection"},
      {ImportImageError::kInvalidVmoIndex, "invalid_vmo_index"},
      {ImportImageError::kImageTooLarge, "image_too_large"},
      {ImportImageError::kCollectionFailed, "collection_failed"},
      {ImportImageError::kGpuImportFailed, "gpu_import_failed"},
      {ImportImageError::kStopped, "stopped"},
      {static_cast<ImportImageError>(999), "unknown"},
  };
  for (const Case& test_case : kCases) {
    SCOPED_TRACE(::testing::Message() << "error=" << static_cast<uint32_t>(test_case.error));
    EXPECT_EQ(ImportImageErrorString(test_case.error), test_case.expected);
  }
}

TEST(ImportWordTest, RoundTripsAllStatesAndFlags) {
  constexpr ImportState kStates[] = {
      ImportState::kNotStarted,  ImportState::kQueued, ImportState::kImporting,
      ImportState::kUnsupported, ImportState::kReady,  ImportState::kFailed,
      ImportState::kReleased,
  };

  EXPECT_EQ(ImportWord::ToBits(ImportWord{}), 0u);

  for (ImportState display : kStates) {
    for (ImportState gpu : kStates) {
      for (bool released : {false, true}) {
        for (bool allocated : {false, true}) {
          SCOPED_TRACE(::testing::Message() << "display=" << static_cast<uint32_t>(display)
                                            << " gpu=" << static_cast<uint32_t>(gpu) << " released="
                                            << released << " allocated=" << allocated);
          const ImportWord word{
              .display = display,
              .gpu = gpu,
              .released = released,
              .allocated = allocated,
          };
          EXPECT_EQ(ImportWord::FromBits(ImportWord::ToBits(word)), word);
        }
      }
    }
  }
}

TEST(ImportWordTest, SatisfiesReadinessMatchesPolicyRules) {
  // `kWaitForImports` requires both display and GPU to be terminal (or `kReleased`).
  EXPECT_FALSE(SatisfiesReadiness(
      ImportWord{.display = ImportState::kReady, .gpu = ImportState::kImporting, .allocated = true},
      ImageReadinessPolicy::kWaitForImports));
  EXPECT_FALSE(SatisfiesReadiness(
      ImportWord{.display = ImportState::kQueued, .gpu = ImportState::kReady, .allocated = true},
      ImageReadinessPolicy::kWaitForImports));
  EXPECT_TRUE(SatisfiesReadiness(
      ImportWord{
          .display = ImportState::kUnsupported, .gpu = ImportState::kReady, .allocated = true},
      ImageReadinessPolicy::kWaitForImports));
  EXPECT_TRUE(SatisfiesReadiness(
      ImportWord{.display = ImportState::kFailed, .gpu = ImportState::kFailed, .allocated = false},
      ImageReadinessPolicy::kWaitForImports));

  // `kReleased` counts as done for its side without `IsTerminalImportState(kReleased)` being true.
  EXPECT_FALSE(SatisfiesReadiness(
      ImportWord{
          .display = ImportState::kReleased, .gpu = ImportState::kImporting, .allocated = true},
      ImageReadinessPolicy::kWaitForImports));
  EXPECT_TRUE(SatisfiesReadiness(
      ImportWord{.display = ImportState::kReleased, .gpu = ImportState::kReady, .allocated = false},
      ImageReadinessPolicy::kWaitForImports));
  EXPECT_FALSE(SatisfiesReadiness(
      ImportWord{
          .display = ImportState::kImporting, .gpu = ImportState::kReleased, .allocated = true},
      ImageReadinessPolicy::kWaitForImports));
  EXPECT_TRUE(SatisfiesReadiness(
      ImportWord{.display = ImportState::kReady, .gpu = ImportState::kReleased, .allocated = false},
      ImageReadinessPolicy::kWaitForImports));

  // `kOptimistic` waits only for `allocated` (or a terminal/released GPU state on failure/cleanup).
  EXPECT_FALSE(SatisfiesReadiness(
      ImportWord{
          .display = ImportState::kNotStarted, .gpu = ImportState::kNotStarted, .allocated = false},
      ImageReadinessPolicy::kOptimistic));
  EXPECT_TRUE(SatisfiesReadiness(
      ImportWord{.display = ImportState::kQueued, .gpu = ImportState::kQueued, .allocated = true},
      ImageReadinessPolicy::kOptimistic));
  EXPECT_TRUE(SatisfiesReadiness(
      ImportWord{.display = ImportState::kFailed, .gpu = ImportState::kFailed, .allocated = false},
      ImageReadinessPolicy::kOptimistic));
  EXPECT_FALSE(SatisfiesReadiness(
      ImportWord{
          .display = ImportState::kFailed, .gpu = ImportState::kNotStarted, .allocated = false},
      ImageReadinessPolicy::kOptimistic));
  EXPECT_TRUE(SatisfiesReadiness(
      ImportWord{
          .display = ImportState::kNotStarted, .gpu = ImportState::kFailed, .allocated = false},
      ImageReadinessPolicy::kOptimistic));
  EXPECT_FALSE(SatisfiesReadiness(
      ImportWord{
          .display = ImportState::kReleased, .gpu = ImportState::kImporting, .allocated = false},
      ImageReadinessPolicy::kOptimistic));
  EXPECT_TRUE(SatisfiesReadiness(
      ImportWord{
          .display = ImportState::kImporting, .gpu = ImportState::kReleased, .allocated = false},
      ImageReadinessPolicy::kOptimistic));
}

TEST(ImportWordTest, TerminalAndGpuCompositionPredicatesCoverAllStatesAndReleasedFlag) {
  constexpr ImportState kStates[] = {
      ImportState::kNotStarted,  ImportState::kQueued, ImportState::kImporting,
      ImportState::kUnsupported, ImportState::kReady,  ImportState::kFailed,
      ImportState::kReleased,
  };

  for (ImportState state : kStates) {
    SCOPED_TRACE(::testing::Message() << "state=" << static_cast<uint32_t>(state));
    const bool expected_terminal = state == ImportState::kReady || state == ImportState::kFailed ||
                                   state == ImportState::kUnsupported;
    EXPECT_EQ(IsTerminalImportState(state), expected_terminal);

    for (bool released : {false, true}) {
      SCOPED_TRACE(::testing::Message() << "released=" << released);
      const bool expected_ready_for_gpu = released || state == ImportState::kReady ||
                                          state == ImportState::kFailed ||
                                          state == ImportState::kReleased;
      EXPECT_EQ(IsReadyForGpuComposition(ImportWord{.gpu = state, .released = released}),
                expected_ready_for_gpu);
    }
  }
}

TEST(ImportedImageLayoutTest, CacheLineAlignmentSeparatesColdFieldsAndPacksWordWithStamp) {
  std::vector<std::shared_ptr<ImportedImage>> heap_images;
  heap_images.reserve(32);

  for (uint64_t i = 1; i <= 32; ++i) {
    SCOPED_TRACE(::testing::Message() << "allocation=" << i);
    auto img = std::make_shared<ImportedImage>(
        MakeTestMetadata(1, i), ImageReadinessPolicy::kWaitForImports, nullptr, nullptr);
    const auto base_addr = reinterpret_cast<uintptr_t>(img.get());
    const auto cold_end = ImageImportTestPeer::LastColdFieldEndAddress(*img);
    const auto word_addr = ImageImportTestPeer::WordAddress(*img);
    const auto stamp_addr = ImageImportTestPeer::FrameStampAddress(*img);

    EXPECT_EQ(base_addr % kCacheLineBytes, 0u);
    EXPECT_EQ(word_addr % kCacheLineBytes, 0u);
    // These two checks document the cache-line separation (`cold_end <= word_addr`
    // and distinct cache lines) guaranteed by member declaration order and
    // `alignas(kCacheLineBytes)`.
    EXPECT_LE(cold_end, word_addr);
    EXPECT_LT((cold_end - 1) / kCacheLineBytes, word_addr / kCacheLineBytes);
    EXPECT_EQ(word_addr / kCacheLineBytes, (stamp_addr + sizeof(uint64_t) - 1) / kCacheLineBytes);

    heap_images.push_back(std::move(img));
  }
}

TEST(ImportedImageTest, TestPeerAccessorsAndMainThreadReads) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  utils::ScopedThreadDispatcherSetter main_setter(loop.dispatcher(), loop.dispatcher());

  auto collection = std::make_shared<ImportedCollection>(42);
  EXPECT_EQ(collection->id(), 42u);
  EXPECT_EQ(collection->gpu_collection(), nullptr);

  auto gpu_collection = std::make_unique<DummyGpuCollection>();
  auto* raw_gpu_collection = gpu_collection.get();
  ImageImportTestPeer::SetGpuCollection(*collection, std::move(gpu_collection));
  EXPECT_EQ(collection->gpu_collection(), raw_gpu_collection);

  constexpr uint32_t kTestTilingType = 7u;
  ImageImportTestPeer::SetCollectionState(*collection, ImportedCollection::State::kDisplayCapable,
                                          kTestTilingType, /*per_buffer_bytes=*/4096);
  EXPECT_EQ(collection->tiling_type(), kTestTilingType);
  EXPECT_EQ(collection->per_buffer_bytes(), 4096u);

  const auto meta = MakeTestMetadata(42, 99);
  auto image =
      std::make_shared<ImportedImage>(meta, ImageReadinessPolicy::kOptimistic, nullptr, nullptr);
  ImageImportTestPeer::SetCollection(*image, collection);
  EXPECT_EQ(image->metadata(), meta);
  EXPECT_EQ(image->collection(), collection);
  EXPECT_EQ(image->policy(), ImageReadinessPolicy::kOptimistic);
  EXPECT_GT(image->created(), zx::time_monotonic(0));
  EXPECT_EQ(image->frame_stamp(), 0u);
  constexpr uint64_t kPresentStamp = 5u | ImportedImage::kNamedByPresent;
  ImageImportTestPeer::SetFrameStamp(*image, kPresentStamp);
  EXPECT_EQ(image->frame_stamp(), kPresentStamp);

  bool gpu_destroyed = false;
  ImageImportTestPeer::SetReadyGpuImage(*image, std::make_unique<DummyGpuImage>(&gpu_destroyed),
                                        ImportState::kReady);

  EXPECT_EQ(image->display_state(), ImportState::kReady);
  EXPECT_EQ(image->tiling_type(), kTestTilingType);
  ASSERT_NE(image->gpu_image(), nullptr);
  EXPECT_FALSE(image->gpu_image()->WasSampled());
  EXPECT_FALSE(gpu_destroyed);

  ImageImportTestPeer::ClearForDestruction(*image);
  EXPECT_TRUE(gpu_destroyed);
  EXPECT_TRUE(image->released());

  ImageImportTestPeer::ClearCollectionForDestruction(*collection);
  EXPECT_EQ(collection->gpu_collection(), nullptr);
}

TEST(ImportedImageDeathTest, DestructorChecksUnclearedGpuImageAndPendingEntries) {
  EXPECT_DEATH(
      {
        ImportedImage image(MakeTestMetadata(), ImageReadinessPolicy::kWaitForImports, nullptr,
                            nullptr);
        ImageImportTestPeer::SetReadyGpuImage(image, std::make_unique<DummyGpuImage>());
      },
      "gpu_image_");

  EXPECT_DEATH(
      {
        ImportedImage image(MakeTestMetadata(), ImageReadinessPolicy::kWaitForImports, nullptr,
                            nullptr);
        ImageImportTestPeer::AddPendingEntry(image, PendingEntry{});
      },
      "pending_");
}

TEST(ImportedImageDeathTest, AccessorChecksRequireReadyStates) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  utils::ScopedThreadDispatcherSetter main_setter(loop.dispatcher(), loop.dispatcher());

  auto collection = std::make_shared<ImportedCollection>(12);
  ImageImportTestPeer::SetCollectionState(*collection, ImportedCollection::State::kDisplayCapable,
                                          /*tiling_type=*/1u, /*per_buffer_bytes=*/4096);
  ImportedImage image(MakeTestMetadata(12, 34), ImageReadinessPolicy::kWaitForImports, nullptr,
                      nullptr);

  EXPECT_DEATH({ (void)image.tiling_type(); }, "display_state");
  EXPECT_DEATH({ (void)image.gpu_image(); }, "gpu_state");

  ImportWord word = image.word();
  word.display = ImportState::kReady;
  ImageImportTestPeer::SetWord(image, word);
  EXPECT_DEATH({ (void)image.tiling_type(); }, "collection_");

  ImageImportTestPeer::SetCollection(image, collection);
  EXPECT_EQ(image.tiling_type(), 1u);

  ImageImportTestPeer::ClearCollectionForDestruction(*collection);
}

TEST(ImportedCollectionDeathTest, DestructorChecksUntornDownState) {
  EXPECT_DEATH(
      {
        ImportedCollection collection(7);
        ImageImportTestPeer::SetGpuCollection(collection, std::make_unique<DummyGpuCollection>());
      },
      "gpu_collection_");

  EXPECT_DEATH(
      {
        ImportedCollection collection(8);
        ImageImportTestPeer::SetCollectionLiveImages(collection, 1);
      },
      "live_images_");
}

TEST(ImportedCollectionDeathTest, AccessorChecksRequireAllocatedStates) {
  ImportedCollection collection(9);
  EXPECT_DEATH({ (void)collection.tiling_type(); }, "kDisplayCapable");
  EXPECT_DEATH({ (void)collection.per_buffer_bytes(); }, "kDisplayCapable");

  ImageImportTestPeer::SetCollectionState(collection, ImportedCollection::State::kGpuOnly,
                                          /*tiling_type=*/0u, /*per_buffer_bytes=*/1024);
  EXPECT_EQ(collection.per_buffer_bytes(), 1024u);
  EXPECT_DEATH({ (void)collection.tiling_type(); }, "kDisplayCapable");

  ImageImportTestPeer::ClearCollectionForDestruction(collection);
}

TEST(DeduplicateImagesTest, ClearsPreExistingOutputAndFiltersNullsInFirstSeenOrder) {
  std::vector<std::shared_ptr<ImportedImage>> pool;
  for (uint64_t i = 1; i <= 32; ++i) {
    pool.push_back(std::make_shared<ImportedImage>(
        MakeTestMetadata(1, i), ImageReadinessPolicy::kWaitForImports, nullptr, nullptr));
  }

  std::pmr::vector<ImportedImage*> out = {pool[0].get(), pool[1].get()};
  DeduplicateImages({}, out);
  EXPECT_TRUE(out.empty());

  const std::vector<ImportedImage*> mixed = {
      nullptr,       pool[2].get(), pool[0].get(), nullptr,       pool[2].get(),
      pool[1].get(), pool[0].get(), nullptr,       pool[3].get(),
  };
  DeduplicateImages(mixed, out);
  const std::pmr::vector<ImportedImage*> expected_mixed = {pool[2].get(), pool[0].get(),
                                                           pool[1].get(), pool[3].get()};
  EXPECT_EQ(out, expected_mixed);

  // Exact 32-entry linear-scan boundary with duplicates and nulls.
  std::vector<ImportedImage*> boundary_32;
  boundary_32.reserve(32);
  std::pmr::vector<ImportedImage*> expected_32;
  for (size_t i = 0; i < 30; ++i) {
    boundary_32.push_back(pool[i].get());
    expected_32.push_back(pool[i].get());
  }
  boundary_32.push_back(nullptr);
  boundary_32.push_back(pool[5].get());
  ASSERT_EQ(boundary_32.size(), 32u);
  DeduplicateImages(boundary_32, out);
  EXPECT_EQ(out, expected_32);
}

TEST(DeduplicateImagesTest, LinearVsHashBoundaryAndPmrArenaRouting) {
  std::vector<std::shared_ptr<ImportedImage>> pool;
  pool.reserve(100);
  for (uint64_t i = 1; i <= 100; ++i) {
    pool.push_back(std::make_shared<ImportedImage>(
        MakeTestMetadata(1, i), ImageReadinessPolicy::kWaitForImports, nullptr, nullptr));
  }

  utils::PmrStatsResource upstream(std::pmr::new_delete_resource());
  utils::PmrStatsResource default_stats(std::pmr::new_delete_resource());
  std::pmr::memory_resource* prev_default = std::pmr::set_default_resource(&default_stats);
  auto restore_default =
      fit::defer([prev_default] { std::pmr::set_default_resource(prev_default); });

  // Case 1: > 32 total entries (40) with <= 32 unique images (4) stays on linear scan and
  // performs only the single `out.reserve()` allocation on `out`'s resource.
  {
    std::vector<ImportedImage*> sparse_40(40, nullptr);
    sparse_40[2] = pool[3].get();
    sparse_40[5] = pool[1].get();
    sparse_40[9] = pool[3].get();
    sparse_40[15] = pool[0].get();
    sparse_40[27] = pool[2].get();
    sparse_40[39] = pool[1].get();

    std::pmr::vector<ImportedImage*> out(&upstream);
    DeduplicateImages(sparse_40, out);

    const std::pmr::vector<ImportedImage*> expected(
        {pool[3].get(), pool[1].get(), pool[0].get(), pool[2].get()},
        std::pmr::new_delete_resource());
    EXPECT_EQ(out, expected);
    EXPECT_EQ(upstream.total_allocation_count(), 1u);
    EXPECT_EQ(default_stats.total_allocation_count(), 0u);
  }
  upstream.Reset();

  // Case 2: 33 unique images (and 40 unique images) with duplicates and nulls transition to the
  // hash set inside the 2 KB `PmrStackArena`, still performing only the single `out.reserve()`
  // allocation on `out`'s resource and zero on the default resource.
  for (size_t unique_count : {size_t{33}, size_t{40}}) {
    SCOPED_TRACE(::testing::Message() << "unique_count=" << unique_count);
    std::vector<ImportedImage*> input;
    std::pmr::vector<ImportedImage*> expected(&upstream);
    expected.reserve(unique_count);
    for (size_t i = 0; i < unique_count; ++i) {
      if (i % 5 == 0) {
        input.push_back(nullptr);
      }
      input.push_back(pool[i].get());
      expected.push_back(pool[i].get());
      if (i % 3 == 0) {
        input.push_back(pool[i].get());
      }
    }
    const size_t baseline_allocs = upstream.total_allocation_count();
    {
      std::pmr::vector<ImportedImage*> out(&upstream);
      DeduplicateImages(input, out);
      EXPECT_EQ(out, expected);
      EXPECT_EQ(upstream.total_allocation_count() - baseline_allocs, 1u);
      EXPECT_EQ(default_stats.total_allocation_count(), 0u);
    }
  }
  upstream.Reset();

  // Case 3: 100 unique images spill the 2 KB stack arena into `out.get_allocator().resource()`
  // while still performing zero allocations on the default resource.
  {
    std::vector<ImportedImage*> input_100;
    input_100.reserve(110);
    for (size_t i = 0; i < 100; ++i) {
      input_100.push_back(pool[i].get());
      if (i < 10) {
        input_100.push_back(pool[i].get());
      }
    }
    std::pmr::vector<ImportedImage*> out(&upstream);
    DeduplicateImages(input_100, out);
    ASSERT_EQ(out.size(), 100u);
    for (size_t i = 0; i < 100; ++i) {
      EXPECT_EQ(out[i], pool[i].get());
    }
    EXPECT_GT(upstream.total_allocation_count(), 1u);
    EXPECT_EQ(default_stats.total_allocation_count(), 0u);
  }
  upstream.Reset();
}

TEST(DeduplicateImagesDeathTest, OverlappingInputAndOutputBufferAborts) {
  auto img = std::make_shared<ImportedImage>(
      MakeTestMetadata(1, 1), ImageReadinessPolicy::kWaitForImports, nullptr, nullptr);
  std::pmr::vector<ImportedImage*> vec = {img.get()};
  EXPECT_DEATH(DeduplicateImages(vec, vec), "overlap");
}

}  // namespace
}  // namespace flatland
