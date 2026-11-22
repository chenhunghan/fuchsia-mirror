// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Design: DESIGN.md in this directory. A change that contradicts a rule
// there amends the rule in the same CL, with the reason (DESIGN.md#changing).

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_ENGINE_IMAGE_IMPORTER_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_ENGINE_IMAGE_IMPORTER_H_

#include <lib/zx/time.h>

#include <cstdint>
#include <span>

#include "src/ui/scenic/lib/display/fidl_typedefs.h"

namespace flatland {

class ImportedImage;

// Reason why a frame went direct to display or fell back to GPU composition,
// ordered by the check sequence in `DisplayCompositor::RenderFrame()`
// (DESIGN.md#metrics).
enum class FrameOutcome : uint8_t {
  kDirect = 0,
  kGpuForced = 1,
  kTooManyLayers = 2,
  kConfigRejected = 3,
  kGpuOnlyImage = 4,
  kDisplayImportFailed = 5,
};

// Per-frame summary passed to `EngineImageImporter::OnFrameCommitted()` after
// `DisplayCompositor::RenderFrame()` commits the frame's display config
// (DESIGN.md#priority).
struct FrameReport {
  FrameOutcome outcome = FrameOutcome::kDirect;
  std::span<ImportedImage* const> images;
  display::WireConfigStamp config_stamp{fuchsia_hardware_display::wire::kInvalidConfigStampValue};
  zx::time_monotonic target_presentation_time{0};
  zx::duration vsync_interval{0};
};

// Frame-facing client image import interface, held by `DisplayCompositor`
// (DESIGN.md#ownership). Every method runs on the main thread only, and every
// method taking images requires unique, non-null images whose `released()` is
// unset.
//
// Main-thread dependency: `ReleaseImageImmediately()` and
// `ReleaseImageImmediately2()` are trusted-session calls that are not
// fence-gated, so an already-published `UberStruct` may still name an image
// they release. Reading `released()` once at the start of `RenderFrame()` holds
// for the entire frame because trusted sessions run on the main thread, every
// other release is fence-gated, and nothing inside `RenderFrame()` pumps main's
// loop (the importer's reply loop runs instead, and cleanup triggered by a
// reply is posted to main's loop).
class EngineImageImporter {
 public:
  virtual ~EngineImageImporter() = default;

  // Whether frames may try direct-to-display composition. When false, every
  // frame composites on the GPU and no display `ImportImage()` calls are made.
  virtual bool SupportsDirectToDisplay() const = 0;

  // Resets the per-frame wait accumulators at the start of a frame.
  virtual void BeginFrame() = 0;

  // For `DisplayCompositor::RenderFrame()`, after the layer-count and config
  // checks pass, before validating the draft layout on the coordinator.
  // Inspects display states without sending imports: returns `kGpuOnlyImage`
  // immediately if any image has display state `kUnsupported` or belongs to a
  // `kGpuOnly` collection, else returns `kDisplayImportFailed` if any image has
  // display state `kFailed` or `kReleased`, has no collection, or belongs to a
  // collection not yet in `kDisplayCapable`, else returns `kDirect`. Falling
  // back when a collection is not yet `kDisplayCapable` guarantees
  // `ImportedCollection::tiling_type()` is valid for every image that passes.
  virtual FrameOutcome CheckDisplayStates(std::span<ImportedImage* const> images) const = 0;

  // For `DisplayCompositor::RenderFrame()`, after draft layout validation
  // succeeds on images that passed `CheckDisplayStates()`. Calls
  // `ImportImage()` for missing display imports and waits until every image
  // reaches a terminal display state: returns `kDirect` when every image
  // reaches display `kReady`, or `kDisplayImportFailed` if any display import
  // fails (DESIGN.md#dispatcher).
  virtual FrameOutcome PrepareForDirectToDisplay(std::span<ImportedImage* const> images) = 0;

  // Ensures every image is GPU `kReady`, `kFailed`, or `kReleased`, running
  // unclaimed jobs inline and waiting only on a job a worker already holds.
  // For images a frame references, `released()` does not change while the
  // frame runs because immediate releases run on main and other releases are
  // fence-gated until no frame references the image.
  virtual void PrepareForGpuComposition(std::span<ImportedImage* const> images) = 0;

  // Called after each frame whose display config was committed, whether
  // composited directly or on the GPU; not called when the commit fails. Updates
  // frame stamps, composition prediction, and per-frame Inspect and trace
  // metrics (DESIGN.md#priority, DESIGN.md#metrics), and starts deferring
  // idle-window display `ImportImage()` calls until this frame is displayed
  // (DESIGN.md#dispatcher).
  virtual void OnFrameCommitted(const FrameReport& report) = 0;

  // Called on every vsync with the stamp of the displayed config. Ends the
  // deferral of idle-window display `ImportImage()` calls once `stamp` reaches
  // the last committed frame's stamp (DESIGN.md#dispatcher), and marks Scenic
  // idle once `idle_after` has elapsed without a frame (DESIGN.md#config).
  virtual void OnVsync(display::WireConfigStamp stamp, zx::time_monotonic timestamp) = 0;
};

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_ENGINE_IMAGE_IMPORTER_H_
