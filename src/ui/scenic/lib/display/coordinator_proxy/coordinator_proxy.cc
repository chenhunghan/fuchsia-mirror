// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Design: DESIGN.md in this directory. A change that contradicts a rule
// there amends the rule in the same CL, with the reason (DESIGN.md#changing).

#include "src/ui/scenic/lib/display/coordinator_proxy/coordinator_proxy.h"

#include <lib/fit/defer.h>
#include <lib/trace/event.h>

#include <algorithm>
#include <string_view>

#include "src/ui/scenic/lib/utils/fidl_array_cast.h"
#include "src/ui/scenic/lib/utils/logging.h"

// Allows us to manually change this to enable logging without *all* Flatland verbose logging.
#define CP_VERBOSE_LOG FLATLAND_VERBOSE_LOG

// At time of writing, `sizeof(DisplayEquivalence) == 104` and `sizeof(DisplayEquivalence) == 56`,
// so we can estimate a 2-layer cache entry to be ~250 bytes; 20*250 == 5KB is a reasonable cost.
static constexpr size_t kCheckConfigCacheSize = 20;

namespace display {

namespace {

// FIDL client `status()` returns the server's epitaph on `kPeerClosedWhileReading`, so peer close
// is mapped to `ZX_ERR_PEER_CLOSED` so that callers can tell a dead coordinator channel apart from
// a method error.
zx_status_t TransportErrorStatus(const fidl::Status& status) {
  if (status.is_peer_closed()) {
    return ZX_ERR_PEER_CLOSED;
  }
  return status.status();
}

bool IsLocalTeardown(const fidl::Status& status) {
  return status.reason() == fidl::Reason::kUnbind || status.is_dispatcher_shutdown();
}

template <typename Result>
zx::result<> UnwrapCoordinatorResult(const Result& result, std::string_view method) {
  if (!result.ok()) {
    if (!IsLocalTeardown(result)) {
      FX_LOGS(ERROR) << method << " transport error: " << result.FormatDescription();
    }
    return zx::error(TransportErrorStatus(result));
  }
  if (result->is_error()) {
    const zx_status_t error_value = result->error_value();
    FX_LOGS(ERROR) << method << " method error: " << zx_status_get_string(error_value);
    return zx::error(error_value);
  }
  return zx::ok();
}

}  // namespace

class CoordinatorProxy::EventHandler final
    : public fidl::WireAsyncEventHandler<fuchsia_hardware_display::Coordinator> {
 public:
  explicit EventHandler(std::shared_ptr<SharedState> state) : state_(std::move(state)) {}
  ~EventHandler() override {
    std::scoped_lock lock(state_->mutex);
    state_->handle = ZX_HANDLE_INVALID;
  }

  void on_fidl_error(fidl::UnbindInfo info) override {
    fit::callback<void(fidl::UnbindInfo)> callback;
    {
      std::scoped_lock lock(state_->mutex);
      state_->handle = ZX_HANDLE_INVALID;
      state_->unbind_info = info;
      callback = std::move(state_->on_fidl_error);
    }
    if (callback) {
      callback(info);
    }
  }

 private:
  std::shared_ptr<SharedState> state_;
};

CoordinatorProxy::CoordinatorProxy(
    fidl::ClientEnd<fuchsia_hardware_display::Coordinator> coordinator,
    async_dispatcher_t* dispatcher, CheckConfigHeuristics check_config_heuristics,
    inspect::Node inspect_node)
    : CoordinatorProxy(
          [&] {
            auto state = std::make_shared<SharedState>();
            {
              std::scoped_lock lock(state->mutex);
              state->handle = coordinator.channel().get();
            }
            fidl::WireSharedClient<fuchsia_hardware_display::Coordinator> client(
                std::move(coordinator), dispatcher, std::make_unique<EventHandler>(state));
            return std::make_pair(std::move(client), std::move(state));
          }(),
          check_config_heuristics, std::move(inspect_node)) {}

CoordinatorProxy::CoordinatorProxy(
    std::pair<fidl::WireSharedClient<fuchsia_hardware_display::Coordinator>,
              std::shared_ptr<SharedState>>
        bound,
    CheckConfigHeuristics check_config_heuristics, inspect::Node inspect_node)
    : CoordinatorProxy(std::move(bound.first), std::move(bound.second), check_config_heuristics,
                       std::move(inspect_node)) {}

CoordinatorProxy::CoordinatorProxy(
    fidl::WireSharedClient<fuchsia_hardware_display::Coordinator> coordinator,
    CheckConfigHeuristics check_config_heuristics, inspect::Node inspect_node)
    : CoordinatorProxy(std::move(coordinator), std::make_shared<SharedState>(),
                       check_config_heuristics, std::move(inspect_node)) {}

CoordinatorProxy::CoordinatorProxy(
    fidl::WireSharedClient<fuchsia_hardware_display::Coordinator> coordinator,
    std::shared_ptr<SharedState> shared_state, CheckConfigHeuristics check_config_heuristics,
    inspect::Node inspect_node)
    : shared_state_(std::move(shared_state)),
      coordinator_(std::move(coordinator)),
      check_config_cache_(kCheckConfigCacheSize),
      check_config_heuristics_(check_config_heuristics),
      inspect_node_(std::move(inspect_node)) {
  FX_DCHECK(coordinator_);
  inspect_api_calls_received_ = inspect_node_.CreateUint("API Calls Received", 0);
  inspect_api_calls_sent_ = inspect_node_.CreateUint("API Calls Sent", 0);
  inspect_apply_config_calls_received_ = inspect_node_.CreateUint("ApplyConfig Calls Received", 0);
  inspect_apply_config_calls_sent_ = inspect_node_.CreateUint("ApplyConfig Calls Sent", 0);
  inspect_check_config_calls_skipped_ = inspect_node_.CreateUint("CheckConfig Calls Skipped", 0);
  inspect_check_config_calls_sent_ = inspect_node_.CreateUint("CheckConfig Calls Sent", 0);
  inspect_check_config_cache_size_ = inspect_node_.CreateUint("CheckConfig Cache Size", 0);
  inspect_import_image_count_ = inspect_node_.CreateUint("Imported Images (total)", 0);
  {
    std::scoped_lock lock(shared_state_->mutex);
    shared_state_->inspect_current_image_count =
        inspect_node_.CreateUint("Imported Images (current)", 0);
  }
  inspect_import_event_count_ = inspect_node_.CreateUint("Imported Events (total)", 0);
  inspect_current_event_count_ = inspect_node_.CreateUint("Imported Events (current)", 0);

// Nobody cares about the specific cache values except when debugging.
#ifndef NDEBUG
  inspect_config_cache_dump_ =
      inspect_node_.CreateLazyValues("CheckConfig Cache values (DEBUG only)", [this] {
        inspect::Inspector inspector;

        std::ostringstream str;
        size_t i = 0;
        for (const auto& kv : check_config_cache_) {
          str << "\n" << i++ << (kv.value ? ": valid " : ": invalid ") << kv.key;
        }
        str << "\n";

        inspector.GetRoot().CreateString("CheckConfig Cache Values", str.str(), &inspector);

        return fpromise::make_ok_promise(std::move(inspector));
      });
#endif
}

CoordinatorProxy::~CoordinatorProxy() {
  std::scoped_lock lock(shared_state_->mutex);
  shared_state_->handle = ZX_HANDLE_INVALID;
  shared_state_->on_fidl_error = nullptr;
  // Reset the property so late asynchronous replies on `coordinator_`'s dispatcher do not access
  // an inspect property whose owning node in this `CoordinatorProxy` has been destroyed.
  shared_state_->inspect_current_image_count = inspect::UintProperty();
}

zx_handle_t CoordinatorProxy::channel_handle() const {
  std::scoped_lock lock(shared_state_->mutex);
  return shared_state_->handle;
}

void CoordinatorProxy::SetOnFidlErrorCallback(fit::callback<void(fidl::UnbindInfo)> callback) {
  std::optional<fidl::UnbindInfo> already_unbound;
  {
    std::scoped_lock lock(shared_state_->mutex);
    FX_CHECK(!callback || !shared_state_->on_fidl_error)
        << "SetOnFidlErrorCallback() cannot overwrite an existing callback without clearing first";
    if (!callback) {
      shared_state_->on_fidl_error = nullptr;
      return;
    }
    if (shared_state_->unbind_info.has_value()) {
      already_unbound = shared_state_->unbind_info;
    } else {
      shared_state_->on_fidl_error = std::move(callback);
      return;
    }
  }
  callback(*already_unbound);
}

zx::result<> CoordinatorProxy::ImportImage(Extent2 image_dimensions, uint32_t image_tiling_type,
                                           WireBufferCollectionId buffer_collection_id,
                                           uint32_t buffer_index, ImageId image_id) {
  TRACE_DURATION("gfx", "display::CoordinatorProxy::ImportImage", "image_id", image_id.value());
  CP_VERBOSE_LOG << "CoordinatorProxy::ImportImage(image_dimensions=" << image_dimensions << ")";

  IncrementImportImageCallsSent();

  WireImageMetadata image_metadata{
      .dimensions = image_dimensions.ToWire(),
      .tiling_type = image_tiling_type,
  };

  // TODO(https://fxbug.dev/580156981): Synchronous `ImportImage()` will be removed once all callers
  // are migrated to the asynchronous overload.
  const auto import_image_result = coordinator_.sync()->ImportImage(
      image_metadata, buffer_collection_id, buffer_index, image_id.ToFidl());
  if (!import_image_result.ok()) {
    FX_LOGS(ERROR) << "ImportImage transport error: " << import_image_result.status_string();
    return zx::error(import_image_result.status());
  }
  if (import_image_result->is_error()) {
    const zx_status_t error_value = import_image_result->error_value();
    FX_LOGS(ERROR) << "ImportImage method error: " << zx_status_get_string(error_value);
    return zx::error(error_value);
  }

  // A duplicate of an already-completed import returns the server's error from the FIDL call
  // above rather than reaching `FX_CHECK(inserted)`, so this check catches only a collision with a
  // pending async import.
  {
    std::scoped_lock lock(shared_state_->mutex);
    const auto [_, inserted] = shared_state_->images.insert(image_id);
    FX_CHECK(inserted) << "Not expecting to find image_id=" << image_id.value();
    shared_state_->UpdateCurrentImageCountLocked();
  }
  return zx::ok();
}

void CoordinatorProxy::ImportImage(Extent2 image_dimensions, uint32_t image_tiling_type,
                                   WireBufferCollectionId buffer_collection_id,
                                   uint32_t buffer_index, ImageId image_id,
                                   ResultCallback on_complete) {
  // Intentionally unguarded: this is a non-per-frame resource import, matching surrounding Scenic
  // trace flows and avoiding conditional `TRACE_FLOW_END()` guards.
  const trace_flow_id_t flow_id = TRACE_NONCE();
  TRACE_DURATION("gfx", "display::CoordinatorProxy::ImportImage", "image_id", image_id.value());
  TRACE_FLOW_BEGIN("gfx", "display::CoordinatorProxy::ImportImage", flow_id);
  CP_VERBOSE_LOG << "CoordinatorProxy::ImportImage(async, image_dimensions=" << image_dimensions
                 << ")";

  IncrementImportImageCallsSent();
  {
    std::scoped_lock lock(shared_state_->mutex);
    const auto [_, inserted] = shared_state_->images.insert(image_id);
    FX_CHECK(inserted) << "Not expecting to find image_id=" << image_id.value();
    shared_state_->UpdateCurrentImageCountLocked();
  }

  WireImageMetadata image_metadata{
      .dimensions = image_dimensions.ToWire(),
      .tiling_type = image_tiling_type,
  };

  // The reply may run on another thread (on `coordinator_`'s dispatcher) and after this
  // `CoordinatorProxy` is destroyed, so the lambda captures only `shared_state_`.
  coordinator_->ImportImage(image_metadata, buffer_collection_id, buffer_index, image_id.ToFidl())
      .ThenExactlyOnce(
          [state = shared_state_, image_id, flow_id, on_complete = std::move(on_complete)](
              fidl::WireUnownedResult<fuchsia_hardware_display::Coordinator::ImportImage>&
                  result) mutable {
            TRACE_DURATION("gfx", "display::CoordinatorProxy::ImportImage[reply]", "image_id",
                           image_id.value());
            TRACE_FLOW_END("gfx", "display::CoordinatorProxy::ImportImage", flow_id);
            zx::result<> status = UnwrapCoordinatorResult(result, "ImportImage");
            if (status.is_error()) {
              std::scoped_lock lock(state->mutex);
              const size_t erased = state->images.erase(image_id);
              FX_CHECK(erased == 1u)
                  << "ImportImage() of image_id=" << image_id.value()
                  << " failed after ReleaseImage() was called while the import was pending";
              state->UpdateCurrentImageCountLocked();
            }
            on_complete(status);
          });
}

void CoordinatorProxy::ReleaseImage(const ImageId& image_id) {
  TRACE_DURATION("gfx", "display::CoordinatorProxy::ReleaseImage", "image_id", image_id.value());
  CP_VERBOSE_LOG << "CoordinatorProxy::ReleaseImage";
  {
    std::scoped_lock lock(shared_state_->mutex);
    const size_t erased = shared_state_->images.erase(image_id);
    FX_CHECK(erased == 1u) << "Expected to find image_id=" << image_id.value();
    shared_state_->UpdateCurrentImageCountLocked();
  }

  // `ReleaseImage()` removes the image from draft state as specified by `coordinator.fidl`, and
  // from applied (committed) state as done by the coordinator driver's `Layer::CleanUpImage()`.
  // Clearing `applied_image_` keeps this proxy's applied state equal to the coordinator's committed
  // state (DESIGN.md#requirements) and keeps `Layer::ResetDraftState()` from copying the released
  // `ImageId` back into `draft_image_`. A displayed layer left without a replacement
  // `SetLayerImage()` then fails fast on `FX_CHECK(draft_image_ != kInvalidImageId)` in
  // `Layer::SendImageDiffToCoordinator()`, instead of committing with a released `image_id`.
  for (auto& [_, layer] : layers_) {
    layer.UnsetImage(image_id);
  }

  fidl::OneWayStatus result = coordinator_->ReleaseImage(image_id.ToFidl());
  if (!result.ok()) {
    FX_LOGS(ERROR) << "Failed to call FIDL ReleaseImage method: " << result.status_string();
  }
}

void CoordinatorProxy::ImportBufferCollection(
    WireBufferCollectionId buffer_collection_id,
    fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token, ResultCallback on_complete) {
  const trace_flow_id_t flow_id = TRACE_NONCE();
  TRACE_DURATION("gfx", "display::CoordinatorProxy::ImportBufferCollection", "collection_id",
                 buffer_collection_id.value);
  TRACE_FLOW_BEGIN("gfx", "display::CoordinatorProxy::ImportBufferCollection", flow_id);
  CP_VERBOSE_LOG << "CoordinatorProxy::ImportBufferCollection(id=" << buffer_collection_id.value
                 << ")";

  coordinator_->ImportBufferCollection(buffer_collection_id, std::move(token))
      .ThenExactlyOnce(
          [collection_id = buffer_collection_id.value, flow_id,
           on_complete = std::move(on_complete)](
              fidl::WireUnownedResult<
                  fuchsia_hardware_display::Coordinator::ImportBufferCollection>& result) mutable {
            TRACE_DURATION("gfx", "display::CoordinatorProxy::ImportBufferCollection[reply]",
                           "collection_id", collection_id);
            TRACE_FLOW_END("gfx", "display::CoordinatorProxy::ImportBufferCollection", flow_id);
            on_complete(UnwrapCoordinatorResult(result, "ImportBufferCollection"));
          });
}

void CoordinatorProxy::SetBufferCollectionConstraints(WireBufferCollectionId buffer_collection_id,
                                                      WireImageBufferUsage buffer_usage,
                                                      ResultCallback on_complete) {
  const trace_flow_id_t flow_id = TRACE_NONCE();
  TRACE_DURATION("gfx", "display::CoordinatorProxy::SetBufferCollectionConstraints",
                 "collection_id", buffer_collection_id.value);
  TRACE_FLOW_BEGIN("gfx", "display::CoordinatorProxy::SetBufferCollectionConstraints", flow_id);
  CP_VERBOSE_LOG << "CoordinatorProxy::SetBufferCollectionConstraints(id="
                 << buffer_collection_id.value << ")";

  coordinator_->SetBufferCollectionConstraints(buffer_collection_id, buffer_usage)
      .ThenExactlyOnce(
          [collection_id = buffer_collection_id.value, flow_id,
           on_complete = std::move(on_complete)](
              fidl::WireUnownedResult<
                  fuchsia_hardware_display::Coordinator::SetBufferCollectionConstraints>&
                  result) mutable {
            TRACE_DURATION("gfx",
                           "display::CoordinatorProxy::SetBufferCollectionConstraints[reply]",
                           "collection_id", collection_id);
            TRACE_FLOW_END("gfx", "display::CoordinatorProxy::SetBufferCollectionConstraints",
                           flow_id);
            on_complete(UnwrapCoordinatorResult(result, "SetBufferCollectionConstraints"));
          });
}

void CoordinatorProxy::ReleaseBufferCollection(WireBufferCollectionId buffer_collection_id) {
  TRACE_DURATION("gfx", "display::CoordinatorProxy::ReleaseBufferCollection", "collection_id",
                 buffer_collection_id.value);
  CP_VERBOSE_LOG << "CoordinatorProxy::ReleaseBufferCollection(id=" << buffer_collection_id.value
                 << ")";

  fidl::OneWayStatus result = coordinator_->ReleaseBufferCollection(buffer_collection_id);
  if (!result.ok()) {
    FX_LOGS(ERROR) << "Failed to call FIDL ReleaseBufferCollection method: "
                   << result.status_string();
  }
}

void CoordinatorProxy::ImportEvent(zx::event event, const EventId& event_id) {
  CP_VERBOSE_LOG << "CoordinatorProxy::ImportEvent";
  IncrementImportEventCallsSent();
  FX_CHECK(!events_.contains(event_id)) << "Not expecting to find event_id=" << event_id.value();

  fidl::OneWayStatus result = coordinator_->ImportEvent(std::move(event), event_id.ToFidl());
  if (!result.ok()) {
    FX_LOGS(ERROR) << "Failed to call FIDL ImportEvent method: " << result.status_string();
  }

  events_.insert(event_id);
  UpdateCurrentEventCount();
}

EventId CoordinatorProxy::ImportEvent(const zx::event& event) {
  static EventId id_generator(1);

  zx::event copied_event;
  zx_status_t status = event.duplicate(ZX_RIGHT_SAME_RIGHTS, &copied_event);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to duplicate display controller event: "
                   << zx_status_get_string(status);
    return kInvalidEventId;
  }

  EventId id = id_generator++;
  ImportEvent(std::move(copied_event), id);
  return id;
}

void CoordinatorProxy::ReleaseEvent(const EventId& event_id) {
  CP_VERBOSE_LOG << "CoordinatorProxy::ReleaseEvent";
  const size_t erased = events_.erase(event_id);
  FX_CHECK(erased == 1u) << "Expected to find event_id=" << event_id.value();
  UpdateCurrentEventCount();

  fidl::OneWayStatus result = coordinator_->ReleaseEvent(event_id.ToFidl());
  if (!result.ok()) {
    FX_LOGS(ERROR) << "Failed to call FIDL ReleaseEvent method: " << result.status_string();
  }
}

LayerId CoordinatorProxy::CreateLayer() {
  CP_VERBOSE_LOG << "CoordinatorProxy::CreateLayer";
  IncrementApiCallsReceived();
  IncrementApiCallsSent();

  LayerId layer_id = next_layer_id_;
  ++next_layer_id_;

  fuchsia_hardware_display::wire::LayerId fidl_layer_id = {layer_id.value()};
  const auto create_layer_result = coordinator_.sync()->CreateLayer(fidl_layer_id);

  if (!create_layer_result.ok()) {
    FX_LOGS(ERROR) << "CreateLayer transport error: " << create_layer_result.status_string();
    return kInvalidLayerId;
  }
  if (create_layer_result->is_error()) {
    FX_LOGS(ERROR) << "CreateLayer method error: "
                   << zx_status_get_string(create_layer_result->error_value());
    return kInvalidLayerId;
  }

  FX_CHECK(!layers_.contains(layer_id));
  layers_.try_emplace(layer_id);
  return layer_id;
}

void CoordinatorProxy::DestroyLayer(const LayerId& layer_id) {
  CP_VERBOSE_LOG << "CoordinatorProxy::DestroyLayer";
  IncrementApiCallsReceived();
  IncrementApiCallsSent();

  FX_CHECK(layers_.contains(layer_id));
  layers_.erase(layer_id);

  // The FIDL API requires that the layer is not in use by an applied/draft state.
  FX_CHECK(std::ranges::find(applied_display_layers_, layer_id) == applied_display_layers_.end());
  FX_CHECK(std::ranges::find(draft_display_layers_, layer_id) == draft_display_layers_.end());

  const fidl::OneWayStatus status = coordinator_->DestroyLayer(layer_id.ToFidl());

  FX_DCHECK(status.ok()) << "Failed to call FIDL DestroyLayer method: " << status.status_string();
}

void CoordinatorProxy::SetDisplayMode(const DisplayId& display_id, const DisplayMode& mode) {
  CP_VERBOSE_LOG << "CoordinatorProxy::SetDisplayMode";
  FX_CHECK(!has_validated_layout_);
  CheckDisplayId(display_id);
  IncrementApiCallsReceived();

  draft_display_mode_ = mode;
}

void CoordinatorProxy::SetDisplayColorConversion(const DisplayId& display_id,
                                                 const std::array<float, 3>& preoffsets,
                                                 const std::array<float, 9>& coefficients,
                                                 const std::array<float, 3>& postoffsets) {
  CP_VERBOSE_LOG << "CoordinatorProxy::SetDisplayColorConversion";
  FX_CHECK(!has_validated_layout_);
  CheckDisplayId(display_id);
  IncrementApiCallsReceived();

  draft_color_conversion_preoffsets_ = preoffsets;
  draft_color_conversion_coefficients_ = coefficients;
  draft_color_conversion_postoffsets_ = postoffsets;
}

void CoordinatorProxy::SetDisplayLayers(const DisplayId& display_id,
                                        const std::span<const LayerId>& layer_ids) {
  CP_VERBOSE_LOG << "CoordinatorProxy::SetDisplayLayers(layer_count=" << layer_ids.size() << ")";
  FX_CHECK(!has_validated_layout_);
  CheckDisplayId(display_id);
  IncrementApiCallsReceived();

  draft_display_layers_.assign(layer_ids.begin(), layer_ids.end());
}

void CoordinatorProxy::SetLayerPrimaryConfig(const LayerId& layer_id,
                                             const Extent2& image_dimensions,
                                             uint32_t image_tiling_type) {
  CP_VERBOSE_LOG << "CoordinatorProxy::SetLayerPrimaryConfig(layer=" << layer_id.value() << ")";
  FX_CHECK(!has_validated_layout_);
  IncrementApiCallsReceived();

  GetLayer(layer_id).SetPrimaryConfig(image_dimensions, image_tiling_type);
}

void CoordinatorProxy::SetLayerPrimaryPosition(const LayerId& layer_id, const RotateFlip& transform,
                                               const Rectangle& image_source,
                                               const Rectangle& display_destination) {
  CP_VERBOSE_LOG << "CoordinatorProxy::SetLayerPrimaryPosition(layer=" << layer_id.value() << ")";
  FX_CHECK(!has_validated_layout_);
  IncrementApiCallsReceived();

  GetLayer(layer_id).SetPrimaryPosition(transform, image_source, display_destination);
}

void CoordinatorProxy::SetLayerPrimaryAlpha(const LayerId& layer_id, const BlendMode& blend_mode,
                                            float alpha_value) {
  CP_VERBOSE_LOG << "CoordinatorProxy::SetLayerPrimaryAlpha(layer=" << layer_id.value() << ")";
  FX_CHECK(!has_validated_layout_);
  IncrementApiCallsReceived();

  GetLayer(layer_id).SetPrimaryAlpha(blend_mode, alpha_value);
}

void CoordinatorProxy::SetLayerImage(const LayerId& layer_id, const ImageId& image_id,
                                     const EventId& wait_event_id) {
  CP_VERBOSE_LOG << "CoordinatorProxy::SetLayerImage(layer=" << layer_id.value()
                 << " image=" << image_id.value() << " wait_event=" << wait_event_id.value() << ")";
  IncrementApiCallsReceived();

  GetLayer(layer_id).SetLayerImage(image_id, wait_event_id);
}

void CoordinatorProxy::SetLayerColorConfig(const LayerId& layer_id, const WireColor& color,
                                           const Rectangle& display_destination) {
  CP_VERBOSE_LOG << "CoordinatorProxy::SetLayerColorConfig(layer=" << layer_id.value() << ")";
  FX_CHECK(!has_validated_layout_);
  IncrementApiCallsReceived();

  GetLayer(layer_id).SetColorConfig(color, display_destination);
}

void CoordinatorProxy::NotifyExpectedPresent(const DisplayId& display_id,
                                             zx::time_monotonic expected_present_time,
                                             zx::duration frame_interval) {
  CP_VERBOSE_LOG << "CoordinatorProxy::NotifyExpectedPresent(display_id=" << display_id.value()
                 << " expected_present_time=" << expected_present_time.get()
                 << " frame_interval=" << frame_interval.get() << ")";
  CheckDisplayId(display_id);
  IncrementApiCallsReceived();
  IncrementApiCallsSent();

  const fidl::OneWayStatus status = coordinator_->NotifyExpectedPresent(
      display_id.ToFidl(), expected_present_time, frame_interval.get());
  if (!status.ok()) {
    FX_LOGS(WARNING) << "Failed to call FIDL NotifyExpectedPresent method: "
                     << status.status_string();
  }
}

zx::result<> CoordinatorProxy::ValidateDraftLayout() {
  CP_VERBOSE_LOG << "CoordinatorProxy::ValidateDraftLayout()";
  // `cache_hit` is attached at the end of the slice because it is known only after the lookup. It
  // is true when `check_config_cache_` held a result (success or failure) for the draft layout.
  TRACE_DURATION_BEGIN("gfx", "display::CoordinatorProxy::ValidateDraftLayout");
  bool cache_hit = false;
  auto end_trace = fit::defer([&cache_hit] {
    TRACE_DURATION_END("gfx", "display::CoordinatorProxy::ValidateDraftLayout", "cache_hit",
                       cache_hit);
  });
  if (has_validated_layout_) {
    return zx::ok();
  }

  // Count every validation attempt here rather than in `ApplyConfig()` so callers that drive
  // `ValidateDraftLayout()` and `CommitValidatedConfig()` directly keep the
  // `ApplyConfig Calls Received` Inspect counter paired with `ApplyConfig Calls Sent`.
  IncrementApplyConfigCallsReceived();

  // Set `temp_display_equivalence_` to have the values we need.
  UpdateDisplayEquivalence();

  // Avoid thrashing the cache with small variations of failing configs.
  if (HeuristicsSayThatCheckConfigWouldFail()) {
    // Check config would fail, so we can return immediately.
    IncrementCheckConfigCallSkipped();
    ResetDraftState();
    return zx::error(ZX_ERR_BAD_STATE);
  }

  // If an equivalent equiv is already in `check_config_cache_` we can skip calling the FIDL method.
  const auto check_config_result = check_config_cache_.Get(temp_display_equivalence_);
  const bool has_cached_check_config = check_config_result.has_value();
  cache_hit = has_cached_check_config;

  if (has_cached_check_config && !check_config_result.value()) {
    // Check config would fail, so we can return immediately.
    TRACE_INSTANT("gfx", "scenic_d2d_failed: cached check config failure", TRACE_SCOPE_THREAD);
    IncrementCheckConfigCallSkipped();
    ResetDraftState();
    return zx::error(ZX_ERR_BAD_STATE);
  }

  // Send only layout diffs before `CheckConfig()`: the coordinator validates a layout before layer
  // images are bound, and skipping `SetLayerImage2()` avoids wasted calls on rejected configs and
  // avoids sending `kInvalidImageId` when images have not yet been imported.
  SendLayoutDiffsToCoordinator();

  if (has_cached_check_config) {
    // Nothing to do: we know `CheckConfig()` would succeed if we called it, because if the cached
    // result was a failure we would have returned early above. This holds even when layout diffs
    // were just sent: the cache key is the layout's equivalence class (for example, alpha is keyed
    // by `AlphaRange`, not its exact value), so equivalent layouts skip the round trip. The
    // coordinator re-checks the unvalidated draft itself at commit (DESIGN.md#cache).
    IncrementCheckConfigCallSkipped();
  } else {
    // There was no cached result, so a round-trip FIDL call is necessary.
    // This result will be cached.
    const WireConfigResult status = FidlCheckConfig();
    if (status != WireConfigResult::kOk) {
      // Cache the failed state and cleanup before returning an error.  Because we already sent the
      // diffs to the display coordinator, we need a FIDL `DiscardConfig()` in addition to resetting
      // the draft state.
      TRACE_INSTANT("gfx", "scenic_d2d_failed: FIDL check config failure", TRACE_SCOPE_THREAD);
      CacheCheckConfigResult(false);
      ResetDraftState();
      FidlDiscardConfig();
      return zx::error(ZX_ERR_BAD_STATE);
    }

    // Cache the successful result; next time we won't need to call the FIDL `CheckConfig()`.
    CacheCheckConfigResult(true);
  }

  has_validated_layout_ = true;
  return zx::ok();
}

void CoordinatorProxy::DiscardValidatedLayout() {
  CP_VERBOSE_LOG << "CoordinatorProxy::DiscardValidatedLayout()";
  TRACE_DURATION("gfx", "display::CoordinatorProxy::DiscardValidatedLayout");
  if (!has_validated_layout_) {
    return;
  }
  has_validated_layout_ = false;

  // `ValidateDraftLayout()` may have sent layout diffs to the coordinator, so aborting calls
  // `DiscardConfig()` in addition to `ResetDraftState()`.
  ResetDraftState();
  FidlDiscardConfig();
}

void CoordinatorProxy::CommitValidatedConfig(const WireConfigStamp& config_stamp) {
  CP_VERBOSE_LOG << "CoordinatorProxy::CommitValidatedConfig(stamp=" << config_stamp.value << ")";
  TRACE_DURATION("gfx", "display::CoordinatorProxy::CommitValidatedConfig");
  FX_CHECK(has_validated_layout_);
  has_validated_layout_ = false;

  SendImageDiffsToCoordinator();
  FidlApplyConfig(config_stamp);
  AcceptDraftState();
}

zx::result<> CoordinatorProxy::ApplyConfig(const WireConfigStamp& config_stamp) {
  CP_VERBOSE_LOG << "CoordinatorProxy::ApplyConfig(stamp=" << config_stamp.value << ")";
  TRACE_DURATION("gfx", "display::CoordinatorProxy::ApplyConfig");

  if (zx::result<> result = ValidateDraftLayout(); result.is_error()) {
    return result;
  }
  CommitValidatedConfig(config_stamp);
  return zx::ok();
}

uint64_t CoordinatorProxy::current_image_count() const {
  std::scoped_lock lock(shared_state_->mutex);
  return shared_state_->images.size();
}

internal::Layer& CoordinatorProxy::GetLayer(const LayerId& layer_id) {
  auto it = layers_.find(layer_id);
  FX_CHECK(it != layers_.end()) << "Layer " << layer_id.value()
                                << " not found in CoordinatorProxy.";
  return it->second;
}

void CoordinatorProxy::CheckDisplayId(const DisplayId& display_id) {
  FX_CHECK(display_id != kInvalidDisplayId);
  FX_CHECK(display_id_ == kInvalidDisplayId || display_id_ == display_id)
      << "New display id=" << display_id.value()
      << " differs from previously-seen display id=" << display_id_.value();
  display_id_ = display_id;
}

void CoordinatorProxy::ResetDraftState() {
  TRACE_DURATION("gfx", "display::CoordinatorProxy::ResetDraftState");

  // Only the layers in the draft display list had their diffs sent; see
  // `SendLayoutDiffsToCoordinator()`. Every other layer keeps its draft, unsent, until it is
  // displayed. Resetting it here would lose a draft that was never sent, such as a color config set
  // once while the layer is undisplayed.
  for (const LayerId& layer_id : draft_display_layers_) {
    GetLayer(layer_id).ResetDraftState();
  }

  draft_display_layers_ = applied_display_layers_;
  draft_display_mode_ = applied_display_mode_;
  draft_color_conversion_preoffsets_ = applied_color_conversion_preoffsets_;
  draft_color_conversion_coefficients_ = applied_color_conversion_coefficients_;
  draft_color_conversion_postoffsets_ = applied_color_conversion_postoffsets_;
}

void CoordinatorProxy::AcceptDraftState() {
  TRACE_DURATION("gfx", "display::CoordinatorProxy::AcceptDraftState");

  applied_display_layers_ = draft_display_layers_;
  applied_display_mode_ = draft_display_mode_;
  applied_color_conversion_preoffsets_ = draft_color_conversion_preoffsets_;
  applied_color_conversion_coefficients_ = draft_color_conversion_coefficients_;
  applied_color_conversion_postoffsets_ = draft_color_conversion_postoffsets_;

  // Only the displayed layers had their diffs sent; see `SendLayoutDiffsToCoordinator()` and
  // `SendImageDiffsToCoordinator()`. Every other layer keeps its draft, unsent, until it is
  // displayed.
  for (const LayerId& layer_id : applied_display_layers_) {
    GetLayer(layer_id).AcceptDraftState();
  }
}

void CoordinatorProxy::SendLayoutDiffsToCoordinator() {
  TRACE_DURATION("gfx", "display::CoordinatorProxy::SendLayoutDiffsToCoordinator");

  // Diffs are sent, and later accepted, only for the layers in the draft display list.  The
  // Coordinator's `CommitConfig()` applies draft changes only to layers on a display, and
  // `DiscardConfig()` drops the rest, so this keeps each layer's applied state in this proxy equal
  // to the Coordinator's committed state.
  //
  // Batch the per-layer counts into a single `IncrementApiCallsSent()` call: each call takes the
  // Inspect `State` mutex and allocates, and the layout/image split would otherwise double the
  // per-layer calls every frame.
  uint64_t layer_api_calls_sent = 0;
  for (const LayerId& layer_id : draft_display_layers_) {
    layer_api_calls_sent += GetLayer(layer_id).SendLayoutDiffsToCoordinator(layer_id, coordinator_);
  }
  if (layer_api_calls_sent > 0) {
    IncrementApiCallsSent(layer_api_calls_sent);
  }

  if (draft_display_layers_ != applied_display_layers_) {
    IncrementApiCallsSent();

    // Safe: see static_asserts in types::IdType<>.
    auto wire_layer_ids = fidl::VectorView<WireLayerId>::FromExternal(
        reinterpret_cast<WireLayerId*>(draft_display_layers_.data()), draft_display_layers_.size());

    const fidl::OneWayStatus status =
        coordinator_->SetDisplayLayers(display_id_.ToFidl(), wire_layer_ids);

    FX_DCHECK(status.ok()) << "Failed to call FIDL SetDisplayLayers method: "
                           << status.status_string();
  }
  if (draft_display_mode_ != applied_display_mode_) {
    IncrementApiCallsSent();

    const fidl::OneWayStatus status =
        coordinator_->SetDisplayMode(display_id_.ToFidl(), draft_display_mode_.ToWire());

    FX_DCHECK(status.ok()) << "Failed to call FIDL SetDisplayMode method: "
                           << status.status_string();
  }

  if (draft_color_conversion_preoffsets_ != applied_color_conversion_preoffsets_ ||
      draft_color_conversion_coefficients_ != applied_color_conversion_coefficients_ ||
      draft_color_conversion_postoffsets_ != applied_color_conversion_postoffsets_) {
    IncrementApiCallsSent();

    const fidl::OneWayStatus status = coordinator_->SetDisplayColorConversion(
        display_id_.ToFidl(),
        utils::ReinterpretStdArrayAsFidlArray(draft_color_conversion_preoffsets_),
        utils::ReinterpretStdArrayAsFidlArray(draft_color_conversion_coefficients_),
        utils::ReinterpretStdArrayAsFidlArray(draft_color_conversion_postoffsets_));

    FX_DCHECK(status.ok()) << "Failed to call FIDL SetDisplayColorConversion method: "
                           << status.status_string();
  }
}

void CoordinatorProxy::SendImageDiffsToCoordinator() {
  TRACE_DURATION("gfx", "display::CoordinatorProxy::SendImageDiffsToCoordinator");

  // Batch per-layer Inspect updates; see `SendLayoutDiffsToCoordinator()`.
  uint64_t layer_api_calls_sent = 0;
  for (const LayerId& layer_id : draft_display_layers_) {
    layer_api_calls_sent += GetLayer(layer_id).SendImageDiffToCoordinator(layer_id, coordinator_);
  }
  if (layer_api_calls_sent > 0) {
    IncrementApiCallsSent(layer_api_calls_sent);
  }
}

void CoordinatorProxy::UpdateDisplayEquivalence() {
  TRACE_DURATION("gfx", "display::CoordinatorProxy::UpdateDisplayEquivalence");

  // This is the only method allowed to mutate `temp_display_equivalence_`.  It is const everywhere
  // else.
  internal::DisplayEquivalence& equiv =
      const_cast<internal::DisplayEquivalence&>(temp_display_equivalence_);

  equiv.layers.clear();
  for (LayerId& layer_id : draft_display_layers_) {
    equiv.layers.push_back(GetLayer(layer_id).draft_equiv());
  }
  equiv.display_mode = draft_display_mode_;
  equiv.color_conversion_preoffsets = draft_color_conversion_preoffsets_;
  equiv.color_conversion_coefficients = draft_color_conversion_coefficients_;
  equiv.color_conversion_postoffsets = draft_color_conversion_postoffsets_;
}

void CoordinatorProxy::FidlApplyConfig(const WireConfigStamp& config_stamp) {
  TRACE_DURATION("gfx", "display::CoordinatorProxy::FidlApplyConfig");
  // The display coordinator ends this flow when it receives `CommitConfig()`. Beginning it here,
  // rather than in `ApplyConfig()`, leaves no flow open when `ApplyConfig()` returns early.
  TRACE_FLOW_BEGIN("gfx", "Display::CommitConfig", config_stamp.value);
  CP_VERBOSE_LOG << "CoordinatorProxy::FidlApplyConfig(stamp=" << config_stamp.value << ")";

  IncrementApplyConfigCallsSent();

  fidl::Arena arena;
  const fidl::OneWayStatus result = coordinator_->CommitConfig(
      fuchsia_hardware_display::wire::CoordinatorCommitConfigRequest::Builder(arena)
          .stamp(config_stamp)
          .Build());

  FX_DCHECK(result.ok()) << "Failed to call FIDL ApplyConfig method: " << result.status_string();
}

void CoordinatorProxy::FidlDiscardConfig() {
  TRACE_DURATION("gfx", "display::CoordinatorProxy::FidlDiscardConfig");

  const fidl::OneWayStatus result = coordinator_->DiscardConfig();
  FX_DCHECK(result.ok()) << "Failed to call FIDL DiscardConfig method: " << result.status_string();
}

WireConfigResult CoordinatorProxy::FidlCheckConfig() {
  TRACE_DURATION("gfx", "display::CoordinatorProxy::FidlCheckConfig");

  IncrementCheckConfigCallsSent();

  const auto status = coordinator_.sync()->CheckConfig();
  FX_DCHECK(status.ok()) << "Failed to call FIDL CheckConfig method: " << status.status_string();

  return status->res;
}

void CoordinatorProxy::CacheCheckConfigResult(bool success) {
  check_config_cache_.Put(temp_display_equivalence_, success);
  inspect_check_config_cache_size_.Set(check_config_cache_.size());
}

bool CoordinatorProxy::HeuristicsSayThatCheckConfigWouldFail() {
  TRACE_DURATION("gfx", "display::CoordinatorProxy::HeuristicsSayThatCheckConfigWouldFail");

  if (!check_config_heuristics_.enable_heuristics) {
    // Heuristics are disabled.
    return false;
  }

  const Extent2& display_extent = temp_display_equivalence_.display_mode.active_area();
  for (const auto& layer : temp_display_equivalence_.layers) {
    // Image layer heuristics.
    if (const display::internal::ImageLayerEquivalence* image_layer =
            std::get_if<display::internal::ImageLayerEquivalence>(&layer.config)) {
      // Images must be displayed at the origin and cover the entire screen.
      // TODO(https://fxbug.dev/446042966): improve equivalences so that we don't need a heuristic.
      auto& display_destination = image_layer->display_destination;
      if (display_destination.x() != 0 || display_destination.y() != 0 ||
          display_destination.width() != display_extent.width() ||
          display_destination.height() != display_extent.height()) {
        TRACE_INSTANT("gfx", "scenic_d2d_failed: heuristic: not full-screen", TRACE_SCOPE_THREAD,
                      "x", TA_INT32(display_destination.x()), "y",
                      TA_INT32(display_destination.y()), "width",
                      TA_INT32(display_destination.width()), "height",
                      TA_INT32(display_destination.height()));
        return true;
      }
    }
  }
  // No heuristic complained about the configuration.
  return false;
}

}  // namespace display

namespace display::internal {

std::ostream& operator<<(std::ostream& str, const DisplayEquivalence& e) {
  str << "DisplayEquivalence";
  str << "\n\tmode = " << e.display_mode;
  str << "\n\tcolor preoffsets = ";
  for (auto offset : e.color_conversion_preoffsets) {
    str << offset << ", ";
  }
  str << "\n\tcolor coefficients = ";
  for (auto coef : e.color_conversion_coefficients) {
    str << coef << ", ";
  }
  str << "\n\tcolor postoffsets = ";
  for (auto offset : e.color_conversion_postoffsets) {
    str << offset << ", ";
  }
  str << "\n\tlayer count = " << e.layers.size() << "\n\tlayers:";
  for (size_t i = 0; i < e.layers.size(); ++i) {
    str << "\n\t\t" << i << ": " << e.layers[i];
  }
  return str;
}

std::ostream& operator<<(std::ostream& str, const LayerEquivalence& le) {
  if (const display::internal::ImageLayerEquivalence* image_le =
          std::get_if<display::internal::ImageLayerEquivalence>(&le.config)) {
    str << *image_le;
    return str;
  }
  if (const display::internal::ColorLayerEquivalence* color_le =
          std::get_if<display::internal::ColorLayerEquivalence>(&le.config)) {
    str << *color_le;
    return str;
  }
  if (const display::internal::UninitializedLayerEquivalence* uninitialized_le =
          std::get_if<display::internal::UninitializedLayerEquivalence>(&le.config)) {
    str << *uninitialized_le;
    return str;
  }
  __UNREACHABLE;
  return str;
}

std::ostream& operator<<(std::ostream& str, const ImageLayerEquivalence& le) {
  str << "ImageLayerEquiv[dst=" << le.display_destination << " src=" << le.image_source
      << " transform=" << le.image_source_transformation << " im_dims=" << le.image_dimensions
      << " tiling=" << le.image_tiling_type << " blend=" << le.blend_mode << " alpha=";
  if (le.alpha_range == ImageLayerEquivalence::AlphaRange::kAlphaOne) {
    str << "1";
  } else if (le.alpha_range == ImageLayerEquivalence::AlphaRange::kAlphaZero) {
    str << "0";
  } else {
    str << "0<a<1";
  }
  str << "]";
  return str;
}

std::ostream& operator<<(std::ostream& str, const ColorLayerEquivalence& le) {
  str << "ColorLayerEquiv[color=";
  auto& bytes = le.color.bytes;
  str << bytes[0] << "," << bytes[1] << "," << bytes[2] << "," << bytes[3] << "," << bytes[4] << ","
      << bytes[5] << "," << bytes[6] << "," << bytes[7];
  str << " format=" << static_cast<uint32_t>(le.color.format) << " dst=" << le.display_destination
      << "]";
  return str;
}

std::ostream& operator<<(std::ostream& str, const UninitializedLayerEquivalence& e) {
  str << "UninitializedLayerEquiv[]";
  return str;
}

}  // namespace display::internal
