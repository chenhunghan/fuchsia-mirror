// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_UNDISPLAYED_FRAME_DEFERRAL_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_UNDISPLAYED_FRAME_DEFERRAL_H_

#include <lib/async/cpp/task.h>
#include <lib/async/dispatcher.h>
#include <lib/fit/function.h>
#include <lib/zx/time.h>

#include <optional>

#include "src/ui/scenic/lib/display/fidl_typedefs.h"

namespace flatland {

// Defers idle-window display `ImportImage()` calls from when a frame's display
// config is committed until `OnVsync()` reports it displayed. The display
// coordinator runs engine imports, render-fence waits, and vsync delivery on one
// dispatcher, so an import in that window can delay a GPU-composited frame's
// `SubmitConfig()` or the `OnVsync()` that signals clients' release fences
// (DESIGN.md#dispatcher). Main thread only.
class UndisplayedFrameDeferral {
 public:
  UndisplayedFrameDeferral(async_dispatcher_t* main_dispatcher, fit::closure on_deadline);

  // Cancels any pending timeout and starts (or replaces) the deferral.
  // Replaces any existing deferral (DESIGN.md#dispatcher: new commit replaces the old one).
  void OnFrameCommitted(display::WireConfigStamp stamp, zx::time_monotonic deadline);

  // Ends the deferral if `displayed` is valid and >= the deferred stamp.
  // Returns true if a deferral was ended this way.
  // Ends the deferral because the frame was displayed (DESIGN.md#dispatcher).
  bool OnStampDisplayed(display::WireConfigStamp displayed);

  // Ends the deferral at once because the frame was skipped.
  void OnFrameSkipped();

  // Returns whether a deferral is active. Lazily ends it if `now >= deadline`
  // because the deadline passed, so a dark display cannot defer calls forever.
  bool IsDeferred(zx::time_monotonic now);

  // Arms the timeout task if a deferral is active and its timeout is not already pending.
  void ArmTimeout();

  // Ends the deferral and cancels the timeout because the importer is stopping.
  void Stop();

 private:
  void Clear();
  void OnDeadlineReached();

  async_dispatcher_t* main_dispatcher_ = nullptr;
  fit::closure on_deadline_;
  std::optional<display::WireConfigStamp> stamp_;
  zx::time_monotonic deadline_{0};
  async::TaskClosure timeout_task_;
};

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_UNDISPLAYED_FRAME_DEFERRAL_H_
