// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Design: DESIGN.md in this directory. A change that contradicts a rule
// there amends the rule in the same CL, with the reason (DESIGN.md#changing).

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_IMAGE_IMPORTER_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_IMAGE_IMPORTER_H_

#include <lib/fit/function.h>
#include <lib/zx/event.h>

#include <cstdint>
#include <memory>
#include <span>
#include <string_view>

#include "src/ui/scenic/lib/allocation/image_metadata.h"
#include "src/ui/scenic/lib/utils/dispatcher_holder.h"

namespace flatland {

class ImportedImage;

// Reason why a client image import failed and closed its session.
enum class ImportImageError : uint32_t {
  kUnknownCollection = 1,
  kInvalidVmoIndex = 2,
  // Image extents fall outside sysmem's `[min_size, max_size]`, or Vulkan's
  // `memory_requirements.size` exceeds the collection's `per_buffer_bytes()`.
  kImageTooLarge = 3,
  kCollectionFailed = 4,
  kGpuImportFailed = 5,
  kStopped = 6,
};

constexpr std::string_view ImportImageErrorString(ImportImageError error) {
  switch (error) {
    case ImportImageError::kUnknownCollection:
      return "unknown_collection";
    case ImportImageError::kInvalidVmoIndex:
      return "invalid_vmo_index";
    case ImportImageError::kImageTooLarge:
      return "image_too_large";
    case ImportImageError::kCollectionFailed:
      return "collection_failed";
    case ImportImageError::kGpuImportFailed:
      return "gpu_import_failed";
    case ImportImageError::kStopped:
      return "stopped";
  }
  return "unknown";
}

// Controls whether presents naming an image wait for its imports to finish
// before `FenceQueue` applies the present (DESIGN.md#requirements).
enum class ImageReadinessPolicy : uint8_t {
  // Present waits until both display and GPU states are terminal.
  kWaitForImports,
  // Present waits only until the image's collection has allocated; frames
  // finish any import they need. Restricted to trusted sessions.
  kOptimistic,
};

// Session-facing client image import interface (DESIGN.md#ownership).
class ImageImporter {
 public:
  using FailureCallback = fit::callback<void(allocation::GlobalImageId, ImportImageError)>;

  virtual ~ImageImporter() = default;

  // Safe from any thread. `metadata.identifier` must be `kInvalidImageId`; the
  // importer assigns a unique ID shared with the display coordinator.
  //
  // Never returns null, even when the import is already known to fail, and
  // never runs `on_failure` inside `ImportImage()`. Instead `on_failure` is
  // posted at most once to `reply_dispatcher` when the collection, VMO index,
  // dimensions, or GPU import fails (DESIGN.md#readiness), even if
  // `ReleaseImage()` was already called, so the handler must not assume the
  // image is still live. A release may instead stop an import before any
  // failure is detected, and then `on_failure` never runs. `reply_dispatcher`
  // and `on_failure` may each be null or empty when the caller needs no failure
  // notification. Because `on_failure` may be destroyed unrun on a worker or
  // when posting to a shut-down loop, everything it captures must be safe to
  // destroy on any thread.
  virtual std::shared_ptr<ImportedImage> ImportImage(
      const allocation::ImageMetadata& metadata, ImageReadinessPolicy policy,
      const std::shared_ptr<utils::DispatcherHolder>& reply_dispatcher,
      FailureCallback on_failure) = 0;

  // Safe from any thread and in any import state (DESIGN.md#owner-acts).
  // Called at most once per image, and the caller must keep a `shared_ptr` to
  // `image` alive across the `ReleaseImage()` call. Resource cleanup runs on
  // the main thread once neither import is in flight.
  virtual void ReleaseImage(const std::shared_ptr<ImportedImage>& image) = 0;

  // Safe from any thread. Ignores null elements in `images`, deduplicates the
  // rest, stamps them for priority, and returns an event signaled with
  // `ZX_EVENT_SIGNALED` once every image satisfies its readiness policy, or an
  // invalid event if all already do. Callers must keep a `shared_ptr` to each
  // non-null image alive across the `SignalWhenReady()` call. Callers pass
  // every image a present names, ready or not, so correctness does not depend
  // on `FenceQueue` applying presents in FIFO order.
  virtual zx::event SignalWhenReady(std::span<ImportedImage* const> images) = 0;
};

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_IMAGE_IMPORTER_H_
