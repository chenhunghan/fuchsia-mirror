// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/image_import/undisplayed_frame_deferral.h"

namespace flatland {

UndisplayedFrameDeferral::UndisplayedFrameDeferral(async_dispatcher_t* main_dispatcher,
                                                   fit::closure on_deadline)
    : main_dispatcher_(main_dispatcher), on_deadline_(std::move(on_deadline)) {
  timeout_task_.set_handler([this] { OnDeadlineReached(); });
}

void UndisplayedFrameDeferral::OnFrameCommitted(display::WireConfigStamp stamp,
                                                zx::time_monotonic deadline) {
  // Replaces any existing deferral (DESIGN.md#dispatcher: new commit replaces the old one).
  Clear();
  stamp_ = stamp;
  deadline_ = deadline;
}

bool UndisplayedFrameDeferral::OnStampDisplayed(display::WireConfigStamp displayed) {
  if (!stamp_.has_value() ||
      displayed.value == fuchsia_hardware_display::wire::kInvalidConfigStampValue ||
      displayed.value < stamp_->value) {
    return false;
  }
  // Ends the deferral because the frame was displayed.
  Clear();
  return true;
}

void UndisplayedFrameDeferral::OnFrameSkipped() {
  // Ends the deferral at once because the frame was skipped.
  Clear();
}

bool UndisplayedFrameDeferral::IsDeferred(zx::time_monotonic now) {
  if (!stamp_.has_value()) {
    return false;
  }
  if (now >= deadline_) {
    // Ends the deferral because the deadline passed, so a dark display cannot defer calls forever.
    Clear();
    return false;
  }
  return true;
}

void UndisplayedFrameDeferral::ArmTimeout() {
  if (stamp_.has_value() && !timeout_task_.is_pending() && deadline_ > zx::time_monotonic{0}) {
    timeout_task_.PostForTime(main_dispatcher_, deadline_);
  }
}

void UndisplayedFrameDeferral::Stop() {
  // Ends the deferral and cancels the timeout because the importer is stopping.
  Clear();
}

void UndisplayedFrameDeferral::Clear() {
  timeout_task_.Cancel();
  stamp_ = std::nullopt;
}

void UndisplayedFrameDeferral::OnDeadlineReached() {
  Clear();
  if (on_deadline_) {
    on_deadline_();
  }
}

}  // namespace flatland
