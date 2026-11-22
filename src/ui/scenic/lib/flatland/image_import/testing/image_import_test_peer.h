// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Design: ../DESIGN.md. A change that contradicts a rule there amends the rule
// in the same CL, with the reason (../DESIGN.md#changing).

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_TESTING_IMAGE_IMPORT_TEST_PEER_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_TESTING_IMAGE_IMPORT_TEST_PEER_H_

#include <fidl/fuchsia.hardware.display.types/cpp/wire.h>

#include <cstdint>
#include <memory>
#include <optional>
#include <utility>

#include "src/ui/scenic/lib/flatland/image_import/imported_collection.h"
#include "src/ui/scenic/lib/flatland/image_import/imported_image.h"

namespace flatland {

// Test-only friend granting unit tests and fakes direct access to
// `ImportedImage` and `ImportedCollection` internals (../DESIGN.md#ownership).
class ImageImportTestPeer {
 public:
  static void SetCollection(ImportedImage& image, std::shared_ptr<ImportedCollection> collection) {
    image.collection_ = std::move(collection);
  }

  static void SetWord(ImportedImage& image, ImportWord word) {
    image.word_.store(ImportWord::ToBits(word), std::memory_order_release);
  }

  static void SetFrameStamp(ImportedImage& image, uint64_t stamp) {
    image.frame_stamp_.store(stamp, std::memory_order_relaxed);
  }

  static void SetReadyGpuImage(ImportedImage& image,
                               std::unique_ptr<ImportedImage::GpuImage> gpu_image,
                               std::optional<ImportState> display_state = std::nullopt) {
    image.gpu_image_ = std::move(gpu_image);
    image.on_failure_ = nullptr;
    image.reply_dispatcher_.reset();
    ImportWord next = image.word();
    if (display_state.has_value()) {
      next.display = *display_state;
    }
    next.gpu = ImportState::kReady;
    next.allocated = true;
    SetWord(image, next);
  }

  static void SetGpuFailed(ImportedImage& image,
                           ImportImageError error = ImportImageError::kGpuImportFailed,
                           std::optional<ImportState> display_state = std::nullopt) {
    image.gpu_image_.reset();
    image.gpu_error_ = error;
    image.on_failure_ = nullptr;
    image.reply_dispatcher_.reset();
    ImportWord next = image.word();
    if (display_state.has_value()) {
      next.display = *display_state;
    }
    next.gpu = ImportState::kFailed;
    SetWord(image, next);
  }

  static std::optional<ImportImageError> GpuError(const ImportedImage& image) {
    return image.gpu_error_;
  }

  static void AddPendingEntry(ImportedImage& image, PendingEntry entry) {
    image.pending_.push_back(std::move(entry));
  }

  static void ClearForDestruction(ImportedImage& image) {
    image.gpu_image_.reset();
    image.pending_.clear();
    image.on_failure_ = nullptr;
    image.reply_dispatcher_.reset();
    SetWord(image, ImportWord{
                       .display = ImportState::kReleased,
                       .gpu = ImportState::kReleased,
                       .released = true,
                       .allocated = image.word().allocated,
                   });
  }

  static uintptr_t WordAddress(const ImportedImage& image) {
    return reinterpret_cast<uintptr_t>(&image.word_);
  }

  static uintptr_t FrameStampAddress(const ImportedImage& image) {
    return reinterpret_cast<uintptr_t>(&image.frame_stamp_);
  }

  static uintptr_t LastColdFieldEndAddress(const ImportedImage& image) {
    return reinterpret_cast<uintptr_t>(&image.policy_) + sizeof(image.policy_);
  }

  static void SetCollectionState(
      ImportedCollection& collection, ImportedCollection::State state,
      uint32_t tiling_type = fuchsia_hardware_display_types::wire::kImageTilingTypeLinear,
      uint64_t per_buffer_bytes = 0, uint32_t buffer_count = 1) {
    collection.tiling_type_ = tiling_type;
    collection.per_buffer_bytes_ = per_buffer_bytes;
    collection.buffer_count_ = buffer_count;
    collection.state_.store(state, std::memory_order_release);
  }

  static void SetGpuCollection(ImportedCollection& collection,
                               std::unique_ptr<ImportedCollection::GpuCollection> gpu_collection) {
    collection.gpu_collection_ = std::move(gpu_collection);
  }

  static void SetCollectionLiveImages(ImportedCollection& collection, uint32_t live_images) {
    collection.live_images_ = live_images;
  }

  static void ClearCollectionForDestruction(ImportedCollection& collection) {
    collection.gpu_collection_.reset();
    collection.waiting_for_allocation_.clear();
    collection.live_images_ = 0;
    collection.state_.store(ImportedCollection::State::kReleased, std::memory_order_release);
  }
};

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_TESTING_IMAGE_IMPORT_TEST_PEER_H_
