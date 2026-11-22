// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Design: DESIGN.md in this directory. A change that contradicts a rule
// there amends the rule in the same CL, with the reason (DESIGN.md#changing).

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_IMPORTED_IMAGE_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_IMPORTED_IMAGE_H_

#include <lib/zx/event.h>
#include <lib/zx/time.h>

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <memory_resource>
#include <mutex>
#include <optional>
#include <span>
#include <utility>
#include <vector>

#include "src/ui/scenic/lib/allocation/id.h"
#include "src/ui/scenic/lib/allocation/image_metadata.h"
#include "src/ui/scenic/lib/flatland/image_import/image_importer.h"
#include "src/ui/scenic/lib/flatland/image_import/imported_collection.h"
#include "src/ui/scenic/lib/utils/dispatcher_holder.h"

namespace flatland {

class ImageImporterImpl;
class ImageImportTestPeer;

// Per-side state of a client image import (DESIGN.md#states).
enum class ImportState : uint8_t {
  kNotStarted = 0,
  kQueued = 1,
  kImporting = 2,
  // Display only: the collection is GPU-only, so no display import runs.
  kUnsupported = 3,
  // For GPU: the bound `vk::Image` is stored in `gpu_image_`; `Render()` wraps
  // it in an `escher::Texture` on first use.
  kReady = 4,
  kFailed = 5,
  // Set by main-thread cleanup or `StopOnMain()` once the per-side resource is destroyed (distinct
  // from `ImportWord::released`).
  kReleased = 6,
};

// Returns true once a side will not transition again before main-thread cleanup
// (DESIGN.md#states).
constexpr bool IsTerminalImportState(ImportState state) {
  return state == ImportState::kReady || state == ImportState::kFailed ||
         state == ImportState::kUnsupported;
}

// Packs both import states plus `released` and `allocated` into a single 32-bit
// atomic word so every multi-field decision is one compare-and-swap
// (DESIGN.md#one-word).
struct ImportWord {
  ImportState display = ImportState::kNotStarted;
  ImportState gpu = ImportState::kNotStarted;
  // Set by `ReleaseImage()` from any thread to request teardown (distinct from
  // `ImportState::kReleased`).
  bool released = false;
  bool allocated = false;

  constexpr bool operator==(const ImportWord&) const = default;

  static constexpr uint32_t ToBits(ImportWord word) {
    return (std::to_underlying(word.display) & 0xFu) |
           ((std::to_underlying(word.gpu) & 0xFu) << 4) |
           (static_cast<uint32_t>(word.released) << 8) |
           (static_cast<uint32_t>(word.allocated) << 9);
  }

  static constexpr ImportWord FromBits(uint32_t bits) {
    return ImportWord{
        .display = static_cast<ImportState>(bits & 0xFu),
        .gpu = static_cast<ImportState>((bits >> 4) & 0xFu),
        .released = ((bits >> 8) & 0x1u) != 0,
        .allocated = ((bits >> 9) & 0x1u) != 0,
    };
  }
};

// Returns true when `word` satisfies `policy` for `SignalWhenReady()`. Failed
// and cleaned-up states unblock waiting presents so a failed import never stalls
// a session's `FenceQueue` (DESIGN.md#owner-acts).
constexpr bool SatisfiesReadiness(ImportWord word, ImageReadinessPolicy policy) {
  const auto display_done =
      IsTerminalImportState(word.display) || word.display == ImportState::kReleased;
  const auto gpu_done = IsTerminalImportState(word.gpu) || word.gpu == ImportState::kReleased;
  switch (policy) {
    case ImageReadinessPolicy::kWaitForImports:
      return display_done && gpu_done;
    case ImageReadinessPolicy::kOptimistic:
      return word.allocated || gpu_done;
  }
  return false;
}

// True once a GPU-composited frame needs no more import work for this image:
// either `released` is set, or `gpu` is `kReady`, `kFailed`, or `kReleased`.
constexpr bool IsReadyForGpuComposition(ImportWord word) {
  return word.released || word.gpu == ImportState::kReady || word.gpu == ImportState::kFailed ||
         word.gpu == ImportState::kReleased;
}

// Per-present countdown state shared via `std::shared_ptr` across each waiting image's
// `PendingEntry` (which `ImportedImage` holds by value in `pending_`); not for renderer use.
// `countdown` is guarded by `ImageImporterImpl::pending_signal_mutex_`.
struct PendingSignal {
  zx::event event;
  size_t countdown = 0;
  zx::time_monotonic created_at{0};
  bool has_wait_for_imports = false;
  // Set only once a present naming a GPU-only image has finished waiting for its imports.
  bool names_gpu_only = false;
};

// One entry per waiting image per `SignalWhenReady()` call, erased under
// `ImageImporterImpl::pending_signal_mutex_` once the owning image's `word_`
// satisfies its `policy_`, or when cleanup runs.
struct PendingEntry {
  std::shared_ptr<PendingSignal> signal;
};

// Assumed CPU cache-line size used to isolate cross-thread atomics from cold fields.
inline constexpr size_t kCacheLineBytes = 64;

// Jointly owned handle for one Flatland client image (DESIGN.md#ownership).
class alignas(kCacheLineBytes) ImportedImage final
    : public std::enable_shared_from_this<ImportedImage> {
 public:
  // Opaque renderer-owned per-image GPU state (DESIGN.md#renderer-contract).
  class GpuImage {
   public:
    virtual ~GpuImage() = default;
    // Main thread only. Returns true once the renderer has wrapped this slot
    // for sampling in a frame; `ImageImporterImpl` uses it as the signal for
    // its wasted-GPU-imports metric. Note: protected slots composited only as
    // black stay false, and a frame that skips rendering after wrapping still sets it.
    //
    // Also decides where the slot may be destroyed: a sampled slot only on the
    // main thread (the renderer may hold main-thread-only state for it), an
    // unsampled slot on any thread (DESIGN.md#renderer-contract).
    virtual bool WasSampled() const { return false; }
  };

  // High bit set on `frame_stamp_` when stamped by `SignalWhenReady()` and
  // cleared when `OnFrameCommitted()` records that a frame used the image.
  static constexpr uint64_t kNamedByPresent = uint64_t{1} << 63;

  ImportedImage(const allocation::ImageMetadata& metadata, ImageReadinessPolicy policy,
                const std::shared_ptr<utils::DispatcherHolder>& reply_dispatcher,
                ImageImporter::FailureCallback on_failure);

  // Unsampled `GpuImage` slots may be destroyed off main before `~ImportedImage()`
  // runs, while sampled `gpu_image_` slots are dropped and `pending_` is drained
  // on the main thread before the last `shared_ptr` can drop on any thread
  // (DESIGN.md#ownership).
  ~ImportedImage();

  ImportedImage(const ImportedImage&) = delete;
  ImportedImage& operator=(const ImportedImage&) = delete;

  const allocation::ImageMetadata& metadata() const { return metadata_; }
  const allocation::GlobalImageId& id() const { return metadata_.identifier; }
  const std::shared_ptr<ImportedCollection>& collection() const { return collection_; }
  ImageReadinessPolicy policy() const { return policy_; }
  zx::time_monotonic created() const { return created_; }

  ImportWord word() const { return ImportWord::FromBits(word_.load(std::memory_order_acquire)); }
  ImportState display_state() const { return word().display; }
  ImportState gpu_state() const { return word().gpu; }
  // True once `ReleaseImage()` has been called (distinct from `ImportState::kReleased`).
  bool released() const { return word().released; }
  uint64_t frame_stamp() const { return frame_stamp_.load(std::memory_order_relaxed); }

  // Main thread only. `tiling_type()` requires `display_state() == ImportState::kReady`
  // on a `kDisplayCapable` collection; `gpu_image()` requires
  // `gpu_state() == ImportState::kReady`. Cleanup never runs inside a frame, so
  // once a frame sees `kReady` these remain valid for the duration of the frame
  // (DESIGN.md#owner-acts).
  uint32_t tiling_type() const;
  GpuImage* gpu_image() const;

 private:
  // The importer reads through the public accessors and uses direct access only to write, or to
  // read outside an accessor's precondition.
  friend class ImageImporterImpl;
  friend class ImageImportTestPeer;

  // Cold metadata, once-written results, and main-only frame counters, isolated
  // from the cross-thread atomic cache line below so steady-state frame reads
  // and counter updates on main never bounce worker or session cache lines
  // (DESIGN.md#one-word).
  const allocation::ImageMetadata metadata_;
  std::shared_ptr<ImportedCollection> collection_;
  // Read by `ImageImporterImpl` (DESIGN.md#metrics) for telemetry and metrics.
  const zx::time_monotonic created_;
  std::unique_ptr<GpuImage> gpu_image_;
  std::optional<ImportImageError> gpu_error_;
  // Monotonic time elapsed until display import succeeded; negative (-1) means not reached yet.
  zx::duration display_ready_latency_{-1};
  // Monotonic time elapsed until GPU import succeeded; negative (-1) means not reached yet.
  zx::duration gpu_ready_latency_{-1};
  // Number of frames that composited this image directly to display.
  uint32_t direct_frames_ = 0;
  // Number of frames that composited this image on the GPU.
  uint32_t gpu_frames_ = 0;
  const ImageReadinessPolicy policy_;

  // Cross-thread state, worker mutex, and present signaling.
  // `alignas(kCacheLineBytes)` guarantees `word_` and `frame_stamp_` share one
  // cache line regardless of `std::make_shared()` control-block prefixes
  // (DESIGN.md#one-word).
  alignas(kCacheLineBytes) std::atomic<uint32_t> word_{ImportWord::ToBits(ImportWord{})};
  std::atomic<uint64_t> frame_stamp_{0};

  // Held for the full duration of the GPU import job so main blocking on an
  // in-flight worker job lends main's deadline via futex priority inheritance
  // (DESIGN.md#requirements).
  std::mutex gpu_run_mutex_;

  // Guarded by `ImageImporterImpl::pending_signal_mutex_`.
  std::vector<PendingEntry> pending_;

  // Both `on_failure_` and `reply_dispatcher_` are moved out before the CAS
  // when GPU enters `kReady` or `kFailed` (DESIGN.md#owner-acts).
  ImageImporter::FailureCallback on_failure_;
  std::weak_ptr<utils::DispatcherHolder> reply_dispatcher_;
};

static_assert(alignof(ImportedImage) == kCacheLineBytes);

// Writes unique non-null images from `images` to `out` in first-seen order
// (`out` is cleared and reserved to `images.size()`, and `images` must not
// overlap `out`'s buffer), switching from a linear scan to a
// `std::pmr::unordered_set` above 32 unique images using a local stack arena
// chained to `out.get_allocator().resource()` (DESIGN.md#readiness).
void DeduplicateImages(std::span<ImportedImage* const> images,
                       std::pmr::vector<ImportedImage*>& out);

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_IMPORTED_IMAGE_H_
