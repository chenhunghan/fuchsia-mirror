// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_BIN_HEALTH_INSPECTOR_H_
#define SRC_UI_SCENIC_BIN_HEALTH_INSPECTOR_H_

#include <lib/fit/function.h>
#include <lib/fit/thread_checker.h>
#include <lib/inspect/cpp/inspect.h>

#include <optional>

#include "src/ui/scenic/lib/display/display_manager.h"
#include "src/ui/scenic/lib/display/display_power_manager.h"
#include "src/ui/scenic/lib/flatland/renderer/omitted_layer_tracker.h"

namespace scenic_impl {

// Handles the creation of the top-level "health" Inspect node and evaluates
// health checks. Under this node is an "overall_health" string that is either
// "UNHEALTHY" or "NOT_UNHEALTHY" (note: not "HEALTHY" because the checks are
// currently far from comprehensive). Active failures add `ERROR_*` child nodes,
// and cumulative `omitted_layers` history remains under `health` after recovery
// to "NOT_UNHEALTHY".
//
// Lifetime requirements: The `DisplayManager`, the `DisplayPowerManager`, and
// any objects captured by `get_omitted_layer_tracker` must outlive this
// instance. A non-null `OmittedLayerTracker` returned by `get_omitted_layer_tracker`
// must remain valid until the Inspect health-check read finishes. `App` guarantees
// these lifetimes by declaring `HealthInspector` as its last member.
//
// Threading: `HealthInspector` construction and Inspect lazy reads (which run
// the health checks directly) must execute on the single thread that owns the
// `DisplayManager`, the `DisplayPowerManager`, and any `OmittedLayerTracker`
// returned by `get_omitted_layer_tracker`. In Scenic, `ComponentInspector` is
// served on `render_loop.dispatcher()`; an Inspect read on any other thread
// aborts.
class HealthInspector {
 public:
  // Getter returning the tracker to evaluate for omitted layers. May be empty (`{}`)
  // or return `nullptr`, either of which skips the omitted-layer check. Any non-null
  // pointer returned must remain valid until the Inspect health-check read finishes.
  using GetOmittedLayerTrackerFn = fit::function<const flatland::OmittedLayerTracker*()>;

  HealthInspector(const std::optional<display::DisplayManager>& display_manager,
                  const std::optional<display::DisplayPowerManager>& display_power_manager,
                  inspect::Node& parent_node, GetOmittedLayerTrackerFn get_omitted_layer_tracker);

  // Non-copyable and non-movable because `CreateLazyValues()` captures `this`.
  HealthInspector(const HealthInspector&) = delete;
  HealthInspector& operator=(const HealthInspector&) = delete;

 private:
  void InitializeInspectObjects(inspect::Node& parent_node);

  // Scenic expects to be receiving vsyncs when the display is powered on.  Otherwise there is
  // probably a bug in the display driver.
  bool CheckReceivingVsyncsWhenDisplayIsOn(inspect::Node& node, zx::time_monotonic now,
                                           inspect::Inspector& inspector);

  // Reports unhealthy while any of the last `flatland::OmittedLayerTracker::kWindowSize`
  // composed frames, however old, omitted a layer, so an idle screen after an omitted
  // frame stays unhealthy until `kWindowSize` more frames are composed. Cumulative
  // omitted-layer history remains in Inspect after recovery.
  bool CheckNoOmittedLayers(inspect::Node& node, inspect::Inspector& inspector) const;

  const std::optional<display::DisplayManager>& display_manager_;
  const std::optional<display::DisplayPowerManager>& display_power_manager_;
  GetOmittedLayerTrackerFn get_omitted_layer_tracker_;
  fit::thread_checker thread_checker_;

  inspect::Node health_node_;
  inspect::LazyNode inspect_lazy_node_;
};

}  // namespace scenic_impl

#endif  // SRC_UI_SCENIC_BIN_HEALTH_INSPECTOR_H_
