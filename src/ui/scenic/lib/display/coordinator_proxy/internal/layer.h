// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_DISPLAY_COORDINATOR_PROXY_INTERNAL_LAYER_H_
#define SRC_UI_SCENIC_LIB_DISPLAY_COORDINATOR_PROXY_INTERNAL_LAYER_H_

#include "src/ui/scenic/lib/display/coordinator_proxy/internal/layer_equivalence.h"
#include "src/ui/scenic/lib/display/fidl_id_types.h"

namespace display::internal {

// Manages the state of a single display layer within the `CoordinatorProxy`.
// This class maintains a "draft" state, representing pending changes, and an "applied" state,
// representing the configuration last known to be successfully applied on the display hardware via
// the FIDL `ApplyConfig()` method.
//
// The `Layer` also holds a `LayerEquivalence`, which is a projection of the layer's state used by
// `CoordinatorProxy` to determine `CheckConfig()` equivalency.
//
// Workflow:
// 1.  Calls to `Set*()` methods update the `draft_equiv_` and other draft fields
//     (e.g., `draft_image_`).
// 2.  If this layer is in the display list, `CoordinatorProxy` calls
//     `SendLayoutDiffsToCoordinator()` before `CheckConfig()`, sending only the layout commands
//     that `CheckConfig()` evaluates.
// 3a. After layout validation succeeds, `CoordinatorProxy` calls `SendImageDiffToCoordinator()`
//     (allowing `SetLayerImage()` to run after layout validation), calls `CommitConfig()`, and
//     calls `AcceptDraftState()` on each displayed `Layer`.  Or:
// 3b. If validation fails or the validated layout is discarded, `CoordinatorProxy` calls
//     `ResetDraftState()` on each displayed `Layer` to revert the draft state back to the last
//     known good applied state.
//
// A layer that is not in the display list neither sends, accepts, nor resets its draft state: it
// keeps the draft until it is displayed.  The Coordinator commits draft changes only for displayed
// layers and `DiscardConfig()` drops the rest, so accepting them would make this applied state
// disagree with the Coordinator's.
class Layer {
 public:
  // Values that are not directly held in a `LayerEquivalence`.  For example, the specific alpha
  // value is irrelevant from the POV of a `LayerEquivalence`, which only cares if the value is one,
  // zero, or between the two.
  struct ConfigValues {
    float alpha_value = 1.f;
  };

  Layer() = default;

  // Corresponds to `fuchsia.hardware.display.Coordinator/SetLayerPrimaryConfig()`.
  // Modifies the layer's draft state; no FIDL methods are invoked.
  //
  // Note: calling this clears the image, as done by the coordinator impl.  It also resets the
  // position and alpha to their defaults, as specified by `coordinator.fidl`.
  void SetPrimaryConfig(const Extent2& image_dimensions, uint32_t image_tiling_type);

  // Corresponds to `fuchsia.hardware.display.Coordinator/SetLayerPrimaryPosition()`.
  // Modifies the layer's draft state; no FIDL methods are invoked.
  void SetPrimaryPosition(const RotateFlip& transform, const Rectangle& src, const Rectangle& dst);

  // Corresponds to `fuchsia.hardware.display.Coordinator/SetLayerPrimaryAlpha()`.
  // Modifies the layer's draft state; no FIDL methods are invoked.
  void SetPrimaryAlpha(const BlendMode& blend_mode, float alpha_value);

  // Corresponds to `fuchsia.hardware.display.Coordinator/SetLayerPrimaryImage()`.
  // Modifies the layer's draft state; no FIDL methods are invoked.
  void SetLayerImage(const ImageId& image_id, const EventId& wait_event_id);

  // Corresponds to `fuchsia.hardware.display.Coordinator/SetLayerColorConfig()`.
  // Modifies the layer's draft state; no FIDL methods are invoked.
  void SetColorConfig(const WireColor& color, const Rectangle& display_destination);

  // If either the draft or applied state includes `image_id`, replace with `kInvalidImageId`.
  void UnsetImage(const ImageId& image_id);

  // Sends only the layout diffs (`SetLayerPrimaryConfig()`, `SetLayerPrimaryPosition()`,
  // `SetLayerPrimaryAlpha()`, or `SetLayerColorConfig()`) needed for `CheckConfig()` to validate
  // the draft layout before layer images are bound. Returns the number of FIDL calls sent.
  size_t SendLayoutDiffsToCoordinator(
      const LayerId& layer_id,
      const fidl::WireSharedClient<fuchsia_hardware_display::Coordinator>& coordinator) const;

  // Sends `SetLayerImage2()` if this is an image layer. Must be called after layout validation
  // succeeds and before `CommitConfig()`; image layers must have a valid `draft_image_` set via
  // `SetLayerImage()`. Returns the number of FIDL calls sent.
  size_t SendImageDiffToCoordinator(
      const LayerId& layer_id,
      const fidl::WireSharedClient<fuchsia_hardware_display::Coordinator>& coordinator) const;

  // Reset all draft state to be identical to the currently-applied state.
  void ResetDraftState();

  // Replace the applied state with the draft state.  Afterward, they are identical.
  void AcceptDraftState();

  const LayerEquivalence& draft_equiv() const { return draft_equiv_; }

 private:
  // Helper for `SendLayoutDiffsToCoordinator()`.
  size_t SendImageLayerLayoutDiffsToCoordinator(
      const LayerId& layer_id,
      const fidl::WireSharedClient<fuchsia_hardware_display::Coordinator>& coordinator) const;

  // Helper for `SendLayoutDiffsToCoordinator()`.
  size_t SendColorLayerDiffsToCoordinator(
      const LayerId& layer_id,
      const fidl::WireSharedClient<fuchsia_hardware_display::Coordinator>& coordinator) const;

  LayerEquivalence applied_equiv_;
  ConfigValues applied_values_;
  ImageId applied_image_ = kInvalidImageId;
  EventId applied_wait_event_ = kInvalidEventId;

  LayerEquivalence draft_equiv_;
  ConfigValues draft_values_;
  ImageId draft_image_ = kInvalidImageId;
  EventId draft_wait_event_ = kInvalidEventId;
};

}  // namespace display::internal

#endif  // SRC_UI_SCENIC_LIB_DISPLAY_COORDINATOR_PROXY_INTERNAL_LAYER_H_
