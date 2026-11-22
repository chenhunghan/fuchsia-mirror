// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Design: DESIGN.md in this directory. A change that contradicts a rule
// there amends the rule in the same CL, with the reason (DESIGN.md#changing).

#include "src/ui/scenic/lib/flatland/image_import/image_importer_impl.h"

#include <fidl/fuchsia.hardware.display.types/cpp/wire.h>
#include <lib/async/cpp/task.h>
#include <lib/async/cpp/time.h>
#include <lib/fit/defer.h>
#include <lib/scheduler/role.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>
#include <lib/zx/clock.h>
#include <lib/zx/thread.h>
#include <zircon/status.h>

#include <algorithm>
#include <cctype>
#include <charconv>
#include <memory_resource>
#include <string>
#include <unordered_set>
#include <utility>

#include "src/ui/scenic/lib/display/util.h"
#include "src/ui/scenic/lib/utils/check_is_on_thread.h"
#include "src/ui/scenic/lib/utils/pmr_stack_arena.h"

namespace flatland {

namespace {

// Safety margin kept before the frame scheduler's next wakeup when deciding whether main's idle
// window has room to start a job (DESIGN.md#dispatcher); 250 us conservatively covers main-thread
// wakeup, task scheduling, and presentation dispatch overhead.
constexpr zx::duration kWakeupSafetyMargin = zx::usec(250);

// High-bit prefix XORed with `GlobalImageId` so per-image trace flows never
// collide with frame numbers or small integer counters (DESIGN.md#metrics).
constexpr uint64_t kImageTraceFlowOffset = 0x494d470000000000ull;

constexpr uint64_t ImageFlowId(allocation::GlobalImageId id) {
  return kImageTraceFlowOffset ^ id.value();
}

constexpr uint64_t kHistFloorUs = 0;
constexpr uint64_t kHistInitialStepUs = 50;
// Reply-handling delays on main are tens of microseconds, finer than import durations.
constexpr uint64_t kReplyDelayHistInitialStepUs = 10;
constexpr uint64_t kHistStepMultiplier = 2;
constexpr size_t kHistBuckets = 16;
// Worker wakeups drain integer job counts starting at 1, capped well below 2^10 jobs per wakeup.
constexpr uint64_t kJobsPerWakeupHistFloor = 0;
constexpr uint64_t kJobsPerWakeupHistInitialStep = 1;
constexpr size_t kJobsPerWakeupHistBuckets = 10;

constexpr const char* ImportKindsString(ImportKinds kinds) {
  switch (kinds) {
    case ImportKinds::kPredictedKindFirst:
      return "predicted_kind_first";
    case ImportKinds::kConcurrent:
      return "concurrent";
  }
  return "unknown";
}

constexpr const char* OffScreenOrderString(OffScreenOrder order) {
  switch (order) {
    case OffScreenOrder::kMostRecentFirst:
      return "most_recent_first";
    case OffScreenOrder::kLeastRecentFirst:
      return "least_recent_first";
  }
  return "unknown";
}

constexpr const char* NeverShownImportsString(NeverShownImports mode) {
  switch (mode) {
    case NeverShownImports::kWhenIdle:
      return "when_idle";
    case NeverShownImports::kWhenFree:
      return "when_free";
    case NeverShownImports::kNever:
      return "never";
  }
  return "unknown";
}

fit::result<std::string, uint32_t> ParseStrictUint32(std::string_view key, std::string_view value) {
  auto err = [&]() {
    return fit::error(std::string("Invalid uint32 value '") + std::string(value) +
                      "' for image_import_policy key '" + std::string(key) + "'");
  };
  if (value.empty() || !std::isdigit(static_cast<unsigned char>(value.front())) ||
      (value.size() > 1 && value.front() == '0')) {
    return err();
  }
  uint32_t parsed = 0;
  const auto [ptr, ec] = std::from_chars(value.data(), value.data() + value.size(), parsed);
  if (ec != std::errc{} || ptr != value.data() + value.size()) {
    return err();
  }
  return fit::ok(parsed);
}

std::optional<fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>> DuplicateTokenSync(
    fidl::SyncClient<fuchsia_sysmem2::BufferCollectionToken>& token,
    allocation::GlobalBufferCollectionId collection_id) {
  TRACE_DURATION("gfx", "fuchsia.sysmem2.BufferCollectionToken::DuplicateSync", "collection_id",
                 collection_id);
  fuchsia_sysmem2::BufferCollectionTokenDuplicateSyncRequest request;
  request.rights_attenuation_masks(std::vector<zx_rights_t>{ZX_RIGHT_SAME_RIGHTS});
  auto result = token->DuplicateSync(request);
  if (result.is_error() || !result->tokens().has_value() || result->tokens()->size() != 1) {
    FX_LOGS(ERROR) << "Failed to duplicate sysmem token for collection " << collection_id;
    return std::nullopt;
  }
  return std::move(result->tokens()->at(0));
}

std::optional<std::vector<fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>>>
CreatePrunableChildren(fidl::UnownedClientEnd<fuchsia_sysmem2::BufferCollectionToken> token,
                       size_t num_children, allocation::GlobalBufferCollectionId collection_id) {
  TRACE_DURATION("gfx", "ImageImporterImpl::CreatePrunableChildren", "collection_id",
                 collection_id);
  auto [group_client_end, group_server_end] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollectionTokenGroup>::Create();
  {
    TRACE_DURATION("gfx", "fuchsia.sysmem2.BufferCollectionToken::CreateBufferCollectionTokenGroup",
                   "collection_id", collection_id);
    fidl::Arena arena;
    const fidl::OneWayStatus status = fidl::WireCall(token)->CreateBufferCollectionTokenGroup(
        fuchsia_sysmem2::wire::BufferCollectionTokenCreateBufferCollectionTokenGroupRequest::
            Builder(arena)
                .group_request(std::move(group_server_end))
                .Build());
    if (!status.ok()) {
      FX_LOGS(ERROR) << "Failed to create sysmem token group: " << status.status_string();
      return std::nullopt;
    }
  }

  fidl::SyncClient<fuchsia_sysmem2::BufferCollectionTokenGroup> group(std::move(group_client_end));
  fuchsia_sysmem2::BufferCollectionTokenGroupCreateChildrenSyncRequest children_req;
  children_req.rights_attenuation_masks(
      std::vector<zx_rights_t>(num_children, ZX_RIGHT_SAME_RIGHTS));
  std::vector<fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>> out_tokens;
  {
    TRACE_DURATION("gfx", "fuchsia.sysmem2.BufferCollectionTokenGroup::CreateChildrenSync",
                   "collection_id", collection_id);
    auto children_res = group->CreateChildrenSync(children_req);
    if (children_res.is_error() || !children_res->tokens().has_value() ||
        children_res->tokens()->size() != num_children) {
      FX_LOGS(ERROR) << "Failed to create sysmem token group children for collection "
                     << collection_id;
      return std::nullopt;
    }
    out_tokens = std::move(children_res->tokens().value());
  }
  fit::result<fidl::OneWayError> all_present_status = fit::ok();
  {
    TRACE_DURATION("gfx", "fuchsia.sysmem2.BufferCollectionTokenGroup::AllChildrenPresent",
                   "collection_id", collection_id);
    all_present_status = group->AllChildrenPresent();
  }
  if (all_present_status.is_error()) {
    FX_LOGS(ERROR) << "AllChildrenPresent failed: "
                   << all_present_status.error_value().FormatDescription();
    return std::nullopt;
  }
  fit::result<fidl::OneWayError> release_status = fit::ok();
  {
    TRACE_DURATION("gfx", "fuchsia.sysmem2.BufferCollectionTokenGroup::Release", "collection_id",
                   collection_id);
    release_status = group->Release();
  }
  if (release_status.is_error()) {
    FX_LOGS(ERROR) << "TokenGroup::Release failed: "
                   << release_status.error_value().FormatDescription();
    return std::nullopt;
  }
  return out_tokens;
}

fidl::Client<fuchsia_sysmem2::BufferCollection> BindEmptyConstraintsParticipant(
    fidl::WireClient<fuchsia_sysmem2::Allocator>& sysmem_allocator,
    fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token, async_dispatcher_t* dispatcher,
    allocation::GlobalBufferCollectionId collection_id) {
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_sysmem2::BufferCollection>::Create();
  {
    TRACE_DURATION("gfx", "fuchsia.sysmem2.Allocator::BindSharedCollection", "collection_id",
                   collection_id);
    fidl::Arena arena;
    const fidl::OneWayStatus status = sysmem_allocator->BindSharedCollection(
        fuchsia_sysmem2::wire::AllocatorBindSharedCollectionRequest::Builder(arena)
            .token(std::move(token))
            .buffer_collection_request(std::move(server_end))
            .Build());
    if (!status.ok()) {
      FX_LOGS(ERROR) << "BindSharedCollection failed: " << status.status_string();
      return {};
    }
  }

  fidl::Client<fuchsia_sysmem2::BufferCollection> client(std::move(client_end), dispatcher);
  {
    TRACE_DURATION("gfx", "fuchsia.sysmem2.BufferCollection::SetConstraints", "collection_id",
                   collection_id);
    if (auto status =
            client->SetConstraints(fuchsia_sysmem2::BufferCollectionSetConstraintsRequest{});
        status.is_error()) {
      FX_LOGS(ERROR) << "SetConstraints failed: " << status.error_value().FormatDescription();
      return {};
    }
  }
  return client;
}

void ReleaseSysmemClient(fidl::Client<fuchsia_sysmem2::BufferCollection>& client) {
  if (client.is_valid()) {
    TRACE_DURATION("gfx", "fuchsia.sysmem2.BufferCollection::Release");
    auto status = client->Release();
    (void)status;
    client = {};
  }
}

void DCheckUniqueUnreleasedImages(std::span<ImportedImage* const> images) {
#ifndef NDEBUG
  if (images.size() <= 32) {
    for (size_t i = 0; i < images.size(); ++i) {
      ImportedImage* img = images[i];
      FX_DCHECK(img != nullptr);
      FX_DCHECK(!img->released());
      for (size_t j = 0; j < i; ++j) {
        FX_DCHECK(images[j] != img) << "Duplicate ImportedImage passed to importer frame method";
      }
    }
  } else {
    utils::PmrStackArena<2048> arena;
    std::pmr::unordered_set<ImportedImage*> seen(arena.resource());
    for (ImportedImage* img : images) {
      FX_DCHECK(img != nullptr);
      FX_DCHECK(!img->released());
      FX_DCHECK(seen.insert(img).second)
          << "Duplicate ImportedImage passed to importer frame method";
    }
  }
#else
  (void)images;
#endif
}

uint64_t ToMicrosClamped(zx::duration d) {
  return d.to_usecs() < 0 ? 0u : static_cast<uint64_t>(d.to_usecs());
}

}  // namespace

// static
uint64_t ImageImporterImpl::SafeCollectionBufferBytes(const ImportedCollection* collection) {
  if (!collection) {
    return 0;
  }
  const ImportedCollection::State s = collection->state();
  return (s == ImportedCollection::State::kDisplayCapable ||
          s == ImportedCollection::State::kGpuOnly)
             ? collection->per_buffer_bytes_
             : 0;
}

void ImageImporterImpl::SysmemParticipants::Abandon() {
  ReleaseSysmemClient(allocation_participant);
  ReleaseSysmemClient(display_participant);
  if (display_token.is_valid()) {
    TRACE_DURATION("gfx", "fuchsia.sysmem2.BufferCollectionToken::Release");
    auto status = fidl::WireCall(display_token)->Release();
    (void)status;
  }
}

// LINT.IfChange
// Parses the comma-separated `key=value` `image_import_policy` grammar.
// Product assembly formats typed `ImageImportConfig` fields into this grammar at build time;
// Scenic rejects any malformed string at startup.
fit::result<std::string> TryParseImageImportPolicy(std::string_view policy,
                                                   ImageImporterImpl::Config& config) {
  if (policy.empty()) {
    return fit::ok();
  }

  std::unordered_set<std::string_view> seen_keys;
  size_t pos = 0;
  while (pos <= policy.size()) {
    const size_t comma = policy.find(',', pos);
    const std::string_view token =
        comma == std::string_view::npos ? policy.substr(pos) : policy.substr(pos, comma - pos);
    if (token.empty()) {
      return fit::error(std::string("Empty token in image_import_policy: '") + std::string(policy) +
                        "'");
    }

    const size_t eq = token.find('=');
    if (eq == std::string_view::npos || eq == 0 || eq + 1 >= token.size()) {
      return fit::error(std::string("Malformed key=value token '") + std::string(token) +
                        "' in image_import_policy");
    }

    const std::string_view key = token.substr(0, eq);
    const std::string_view value = token.substr(eq + 1);

    if (!seen_keys.insert(key).second) {
      return fit::error(std::string("Duplicate key '") + std::string(key) +
                        "' in image_import_policy");
    }

    if (key == "kinds") {
      if (value == "predicted_kind_first") {
        config.import_kinds = ImportKinds::kPredictedKindFirst;
      } else if (value == "concurrent") {
        config.import_kinds = ImportKinds::kConcurrent;
      } else {
        return fit::error(std::string("Unknown kinds value '") + std::string(value) +
                          "' in image_import_policy");
      }
    } else if (key == "off_screen") {
      if (value == "most_recent_first") {
        config.off_screen_order = OffScreenOrder::kMostRecentFirst;
      } else if (value == "least_recent_first") {
        config.off_screen_order = OffScreenOrder::kLeastRecentFirst;
      } else {
        return fit::error(std::string("Unknown off_screen value '") + std::string(value) +
                          "' in image_import_policy");
      }
    } else if (key == "never_shown") {
      if (value == "when_idle") {
        config.never_shown_imports = NeverShownImports::kWhenIdle;
      } else if (value == "when_free") {
        config.never_shown_imports = NeverShownImports::kWhenFree;
      } else if (value == "never") {
        config.never_shown_imports = NeverShownImports::kNever;
      } else {
        return fit::error(std::string("Unknown never_shown value '") + std::string(value) +
                          "' in image_import_policy");
      }
    } else if (key == "idle_ms") {
      auto parsed = ParseStrictUint32(key, value);
      if (parsed.is_error()) {
        return parsed.take_error();
      }
      config.idle_after = zx::msec(parsed.value());
    } else if (key == "display_cap") {
      auto parsed = ParseStrictUint32(key, value);
      if (parsed.is_error()) {
        return parsed.take_error();
      }
      if (parsed.value() < 1) {
        return fit::error("display_cap in image_import_policy must be >= 1");
      }
      config.display_import_cap = parsed.value();
    } else if (key == "reply_thread") {
      if (value == "false") {
        config.display_reply_thread = false;
      } else if (value == "true") {
        config.display_reply_thread = true;
      } else {
        return fit::error(std::string("Unknown reply_thread value '") + std::string(value) +
                          "' in image_import_policy");
      }
    } else {
      return fit::error(std::string("Unknown key '") + std::string(key) +
                        "' in image_import_policy");
    }

    if (comma == std::string_view::npos) {
      break;
    }
    pos = comma + 1;
  }
  return fit::ok();
}

void ParseImageImportPolicy(std::string_view policy, ImageImporterImpl::Config& config) {
  auto result = TryParseImageImportPolicy(policy, config);
  FX_CHECK(result.is_ok()) << result.error_value();
}

std::string FormatImageImportPolicy(const ImageImporterImpl::Config& config) {
  return std::string("kinds=") + ImportKindsString(config.import_kinds) +
         ",off_screen=" + OffScreenOrderString(config.off_screen_order) +
         ",never_shown=" + NeverShownImportsString(config.never_shown_imports) +
         ",idle_ms=" + std::to_string(config.idle_after.to_msecs()) +
         ",display_cap=" + std::to_string(config.display_import_cap) +
         ",reply_thread=" + (config.display_reply_thread ? "true" : "false");
}
// LINT.ThenChange(/src/lib/assembly/platform_configuration/src/subsystems/ui.rs,
// /src/lib/assembly/config_schema/src/platform_settings/ui_config.rs,
// /src/ui/scenic/bin/meta/config.shard.cml:image_import_policy,
// /src/ui/scenic/lib/flatland/image_import/tests/image_importer_impl_unittest.cc:policy_test_cases)

// static
std::shared_ptr<ImageImporterImpl> ImageImporterImpl::New(
    Config config, async_dispatcher_t* main_dispatcher,
    std::shared_ptr<display::CoordinatorProxy> coordinator, async::Loop& reply_loop,
    std::shared_ptr<Renderer> renderer, fit::function<std::optional<zx::time>()> next_wakeup,
    inspect::Node inspect_node, std::string_view raw_policy) {
  return std::shared_ptr<ImageImporterImpl>(new ImageImporterImpl(
      config, main_dispatcher, std::move(coordinator), reply_loop, std::move(renderer),
      std::move(next_wakeup), std::move(inspect_node), raw_policy));
}

ImageImporterImpl::ImageImporterImpl(Config config, async_dispatcher_t* main_dispatcher,
                                     std::shared_ptr<display::CoordinatorProxy> coordinator,
                                     async::Loop& reply_loop, std::shared_ptr<Renderer> renderer,
                                     fit::function<std::optional<zx::time>()> next_wakeup,
                                     inspect::Node inspect_node, std::string_view raw_policy)
    : config_(config),
      main_dispatcher_(main_dispatcher),
      undisplayed_frame_deferral_(main_dispatcher, [this] { ScheduleDispatcher(); }),
      coordinator_(std::move(coordinator)),
      renderer_(std::move(renderer)),
      next_wakeup_(std::move(next_wakeup)),
      reply_loop_(reply_loop),
      watch_state_(std::make_shared<ChannelWatchState>()),
      last_frame_time_(async::Now(main_dispatcher)),
      inspect_node_(std::move(inspect_node)) {
  FX_CHECK(utils::IsOnMainThread());
  FX_CHECK(main_dispatcher_ != nullptr);
  FX_CHECK(renderer_ != nullptr);
  FX_CHECK(next_wakeup_ != nullptr);
  FX_CHECK(config_.display_import_cap >= 1);
  FX_CHECK(config_.idle_after >= zx::duration{0});
  if (config_.enable_direct_to_display) {
    FX_CHECK(coordinator_ != nullptr);
  } else {
    last_outcome_.store(FrameOutcome::kGpuForced, std::memory_order_relaxed);
  }

  if (config_.display_reply_thread) {
    const zx_status_t status = reply_loop_.StartThread("scenic.display-reply");
    FX_CHECK(status == ZX_OK) << "Failed to start scenic.display-reply thread: "
                              << zx_status_get_string(status);
    async::PostTask(reply_loop_.dispatcher(), [] {
      const zx_status_t role_status =
          fuchsia_scheduler::SetRoleForThisThread("fuchsia.scenic.display-reply");
      if (role_status != ZX_OK) {
        FX_LOGS(WARNING) << "Failed to apply fuchsia.scenic.display-reply role: "
                         << zx_status_get_string(role_status);
      }
    });
  }

  wakeup_retry_task_.set_handler([this] { ScheduleDispatcher(); });
  main_dispatcher_gate_ = std::make_shared<MainDispatcherGate>();
  main_dispatcher_gate_->dispatcher = main_dispatcher_;

  if (!config_.display_reply_thread && coordinator_) {
    coordinator_->SetOnFidlErrorCallback(
        [weak_watch = std::weak_ptr<ChannelWatchState>(watch_state_)](fidl::UnbindInfo) {
          if (auto watch = weak_watch.lock()) {
            watch->channel_closed.store(true, std::memory_order_relaxed);
          }
        });
    const zx_handle_t handle = coordinator_->channel_handle();
    if (handle == ZX_HANDLE_INVALID) {
      watch_state_->channel_closed.store(true, std::memory_order_relaxed);
    } else if (!watch_state_->channel_closed.load(std::memory_order_relaxed)) {
      watch_state_->readable_wait.set_object(handle);
      watch_state_->readable_wait.set_trigger(ZX_CHANNEL_READABLE | ZX_CHANNEL_PEER_CLOSED);
      watch_state_->readable_wait.set_options(ZX_WAIT_ASYNC_TIMESTAMP);
      watch_state_->readable_wait.set_handler(
          [this](async_dispatcher_t* d, async::Wait* w, zx_status_t s,
                 const zx_packet_signal_t* sig) { OnReplyChannelReadable(d, w, s, sig); });
      const zx_status_t wait_status = watch_state_->readable_wait.Begin(main_dispatcher_);
      FX_CHECK(wait_status == ZX_OK)
          << "Failed to arm coordinator reply wait: " << zx_status_get_string(wait_status);
    }
  }

  InitInspect(raw_policy);

  gpu_workers_.reserve(config_.worker_pool_size);
  for (uint8_t i = 0; i < config_.worker_pool_size; ++i) {
    gpu_workers_.emplace_back([this, i] { GpuWorkerLoop(i); });
  }
}

ImageImporterImpl::~ImageImporterImpl() {
  FX_CHECK(utils::IsOnMainThread());
  FX_CHECK(stopped_) << "ImageImporterImpl::StopOnMain() must run before destruction";
}

void ImageImporterImpl::InitInspect(std::string_view raw_policy) {
  inspect_config_node_ = inspect_node_.CreateChild("config");
  inspect_config_node_.RecordString("image_import_policy", std::string(raw_policy));
  inspect_config_node_.RecordBool("enable_direct_to_display", config_.enable_direct_to_display);
  inspect_config_node_.RecordString("import_kinds", ImportKindsString(config_.import_kinds));
  inspect_config_node_.RecordString("off_screen_order",
                                    OffScreenOrderString(config_.off_screen_order));
  inspect_config_node_.RecordString("never_shown_imports",
                                    NeverShownImportsString(config_.never_shown_imports));
  inspect_config_node_.RecordUint("idle_after_ms",
                                  static_cast<uint64_t>(config_.idle_after.to_msecs()));
  inspect_config_node_.RecordUint("display_import_cap", config_.display_import_cap);
  inspect_config_node_.RecordUint("worker_pool_size", config_.worker_pool_size);
  inspect_config_node_.RecordBool("display_reply_thread", config_.display_reply_thread);

  inspect_image_imports_rejected_ = inspect_node_.CreateUint("image_imports_rejected", 0);
  inspect_display_imports_failed_ = inspect_node_.CreateUint("display_imports_failed", 0);
  inspect_gpu_imports_failed_ = inspect_node_.CreateUint("gpu_imports_failed", 0);
  inspect_collection_allocations_failed_ =
      inspect_node_.CreateUint("collection_allocations_failed", 0);
  inspect_prepare_for_gpu_ns_ = inspect_node_.CreateUint("prepare_for_gpu_composition_ns", 0);
  inspect_wasted_display_imports_ = inspect_node_.CreateUint("wasted_display_imports", 0);
  inspect_wasted_gpu_imports_ = inspect_node_.CreateUint("wasted_gpu_imports", 0);
  inspect_frames_direct_ = inspect_node_.CreateUint("frames_direct", 0);
  inspect_frames_gpu_forced_ = inspect_node_.CreateUint("frames_gpu_forced", 0);
  inspect_frames_fallback_too_many_layers_ =
      inspect_node_.CreateUint("frames_fallback_too_many_layers", 0);
  inspect_frames_fallback_config_rejected_ =
      inspect_node_.CreateUint("frames_fallback_config_rejected", 0);
  inspect_frames_fallback_gpu_only_image_ =
      inspect_node_.CreateUint("frames_fallback_gpu_only_image", 0);
  inspect_frames_fallback_display_import_failed_ =
      inspect_node_.CreateUint("frames_fallback_display_import_failed", 0);
  inspect_frames_skipped_ = inspect_node_.CreateUint("frames_skipped", 0);
  inspect_predictions_correct_ = inspect_node_.CreateUint("predictions_correct", 0);
  inspect_predictions_wrong_ = inspect_node_.CreateUint("predictions_wrong", 0);
  inspect_predictions_from_present_hint_ =
      inspect_node_.CreateUint("predictions_from_present_hint", 0);
  inspect_predictions_from_present_hint_correct_ =
      inspect_node_.CreateUint("predictions_from_present_hint_correct", 0);
  inspect_worker_wakeups_ = inspect_node_.CreateUint("worker_wakeups", 0);
  inspect_worker_jobs_completed_ = inspect_node_.CreateUint("worker_jobs_completed", 0);

  inspect_frame_blocked_total_us_ = inspect_node_.CreateExponentialUintHistogram(
      "frame_blocked_total_us", kHistFloorUs, kHistInitialStepUs, kHistStepMultiplier,
      kHistBuckets);
  inspect_frame_blocked_display_sent_us_ = inspect_node_.CreateExponentialUintHistogram(
      "frame_blocked_display_sent_us", kHistFloorUs, kHistInitialStepUs, kHistStepMultiplier,
      kHistBuckets);
  inspect_frame_blocked_display_in_flight_us_ = inspect_node_.CreateExponentialUintHistogram(
      "frame_blocked_display_in_flight_us", kHistFloorUs, kHistInitialStepUs, kHistStepMultiplier,
      kHistBuckets);
  inspect_frame_blocked_gpu_us_ = inspect_node_.CreateExponentialUintHistogram(
      "frame_blocked_gpu_us", kHistFloorUs, kHistInitialStepUs, kHistStepMultiplier, kHistBuckets);
  inspect_image_wait_display_us_ = inspect_node_.CreateExponentialUintHistogram(
      "image_wait_display_us", kHistFloorUs, kHistInitialStepUs, kHistStepMultiplier, kHistBuckets);
  inspect_image_wait_gpu_us_ = inspect_node_.CreateExponentialUintHistogram(
      "image_wait_gpu_us", kHistFloorUs, kHistInitialStepUs, kHistStepMultiplier, kHistBuckets);
  inspect_display_reply_handling_delay_us_ = inspect_node_.CreateExponentialUintHistogram(
      "display_reply_handling_delay_us", kHistFloorUs, kReplyDelayHistInitialStepUs,
      kHistStepMultiplier, kHistBuckets);
  inspect_present_delay_wait_for_imports_us_ = inspect_node_.CreateExponentialUintHistogram(
      "present_delay_wait_for_imports_us", kHistFloorUs, kHistInitialStepUs, kHistStepMultiplier,
      kHistBuckets);
  inspect_present_delay_optimistic_us_ = inspect_node_.CreateExponentialUintHistogram(
      "present_delay_optimistic_us", kHistFloorUs, kHistInitialStepUs, kHistStepMultiplier,
      kHistBuckets);
  inspect_worker_jobs_per_wakeup_ = inspect_node_.CreateExponentialUintHistogram(
      "worker_jobs_per_wakeup", kJobsPerWakeupHistFloor, kJobsPerWakeupHistInitialStep,
      kHistStepMultiplier, kJobsPerWakeupHistBuckets);

  inspect_live_metrics_ = inspect_node_.CreateLazyValues("live_state", [this] {
    uint64_t live_count = 0;
    uint64_t total_bytes = 0;
    uint64_t queued_display = 0;
    uint64_t queued_gpu = 0;
    uint64_t running_gpu = 0;
    {
      std::scoped_lock lock(collections_mutex_);
      live_count = live_images_by_id_.size();
      for (const auto& [_, img] : live_images_by_id_) {
        if (img->word().allocated) {
          total_bytes += SafeCollectionBufferBytes(img->collection().get());
        }
      }
    }
    {
      std::scoped_lock lock(queue_mutex_);
      queued_display = QueuedJobCountLocked(JobKind::kDisplay);
      queued_gpu = QueuedJobCountLocked(JobKind::kGpu);
      running_gpu = gpu_jobs_running_;
    }

    inspect::Inspector inspector;
    auto& root = inspector.GetRoot();
    root.RecordUint("live_images", live_count);
    root.RecordUint("total_imported_bytes", total_bytes);
    root.RecordUint("queued_display_jobs", queued_display);
    root.RecordUint("queued_gpu_jobs", queued_gpu);
    root.RecordUint("in_flight_gpu_jobs", running_gpu);
    root.RecordUint("in_flight_display_imports",
                    display_imports_in_flight_.load(std::memory_order_relaxed));
    return fpromise::make_ok_promise(std::move(inspector));
  });
}

// static
std::optional<std::pair<ImportWord, ImportWord>> ImageImporterImpl::Transition(
    ImportedImage& image, fit::function<bool(ImportWord&)> mutate) {
  uint32_t old_bits = image.word_.load(std::memory_order_acquire);
  for (;;) {
    ImportWord next = ImportWord::FromBits(old_bits);
    if (!mutate(next)) {
      return std::nullopt;
    }
    if (image.word_.compare_exchange_weak(old_bits, ImportWord::ToBits(next),
                                          std::memory_order_acq_rel, std::memory_order_acquire)) {
      return std::make_pair(ImportWord::FromBits(old_bits), next);
    }
  }
}

void ImageImporterImpl::StopOnMain() {
  FX_CHECK(utils::IsOnMainThread());
  if (stopped_) {
    return;
  }
  TRACE_DURATION("gfx", "ImageImporterImpl::StopOnMain");

  {
    std::scoped_lock gate_lock(main_dispatcher_gate_->mutex);
    main_dispatcher_gate_->dispatcher = nullptr;
  }
  // `collections_mutex_` fences `ImportImage()`'s registry insertion and `queue_mutex_` fences
  // the `gpu_queue_nonempty_.wait()` predicate before `notify_all()`.
  {
    std::scoped_lock lock(collections_mutex_, queue_mutex_);
    stopping_.store(true, std::memory_order_relaxed);
  }
  gpu_queue_nonempty_.notify_all();
  for (std::thread& worker : gpu_workers_) {
    if (worker.joinable()) {
      worker.join();
    }
  }
  gpu_workers_.clear();
  {
    // Destroy anything the workers did not reap before they stopped.
    std::scoped_lock lock(queue_mutex_);
    reaper_queue_.clear();
  }

  // Drain any in-flight display imports so no coordinator callback can touch an
  // image or `this` after cleanup runs (DESIGN.md#cleanup-on-main).
  if (stop_drain_entered_hook_for_test_) {
    stop_drain_entered_hook_for_test_();
  }
  if (!config_.display_reply_thread) {
    watch_state_->readable_wait.Cancel();
    while (display_imports_outstanding_.load(std::memory_order_acquire) > 0) {
      const zx_status_t status = reply_loop_.Run(zx::time::infinite(), /*once=*/true);
      FX_CHECK(status == ZX_OK) << "reply_loop must remain running during StopOnMain: "
                                << zx_status_get_string(status);
    }
  } else {
    std::unique_lock<std::mutex> lock(pending_signal_mutex_);
    display_import_done_.wait(
        lock, [this] { return display_imports_outstanding_.load(std::memory_order_acquire) == 0; });
  }

  // Cancel any collections still awaiting coordinator constraint setup so their
  // `GpuCollection` slots release Vulkan handles before Escher tears down.
  auto pending_to_abandon = std::move(pending_registrations_);
  pending_registrations_.clear();
  for (auto& [collection_id, pending] : pending_to_abandon) {
    AbandonRegistration(collection_id, *pending);
  }

  std::vector<std::shared_ptr<ImportedImage>> images_to_clean;
  {
    std::scoped_lock lock(collections_mutex_);
    images_to_clean.reserve(live_images_by_id_.size());
    for (const auto& [_, img] : live_images_by_id_) {
      images_to_clean.push_back(img);
    }
  }
  for (const auto& img : images_to_clean) {
    CleanUp(img);
  }

  std::vector<std::shared_ptr<ImportedCollection>> collections_to_tear_down;
  {
    std::scoped_lock lock(collections_mutex_);
    collections_to_tear_down.reserve(collections_.size());
    for (auto& [_, coll] : collections_) {
      collections_to_tear_down.push_back(std::move(coll));
    }
    collections_.clear();
  }
  for (const auto& coll : collections_to_tear_down) {
    TearDown(*coll);
  }

  {
    std::scoped_lock lock(queue_mutex_);
    display_queue_.clear();
    gpu_queue_.clear();
    EmitQueueTraceCountersLocked();
  }

  dispatch_posted_.store(false, std::memory_order_relaxed);
  wakeup_retry_task_.Cancel();
  undisplayed_frame_deferral_.Stop();
  watch_state_->channel_closed.store(true, std::memory_order_relaxed);
  watch_state_->readable_wait.Cancel();
  if (coordinator_) {
    coordinator_->SetOnFidlErrorCallback(nullptr);
    coordinator_.reset();
  }
  stopped_ = true;
}

std::optional<ImageImporterImpl::SysmemParticipants> ImageImporterImpl::SetupSysmemParticipants(
    allocation::GlobalBufferCollectionId collection_id,
    fidl::WireClient<fuchsia_sysmem2::Allocator>& sysmem_allocator,
    fidl::SyncClient<fuchsia_sysmem2::BufferCollectionToken>& parent_token) {
  SysmemParticipants parts;
  if (config_.enable_direct_to_display) {
    auto children = CreatePrunableChildren(parent_token.client_end(), 2, collection_id);
    if (!children.has_value()) {
      {
        TRACE_DURATION("gfx", "fuchsia.sysmem2.BufferCollectionToken::Release", "collection_id",
                       collection_id);
        auto release_parent = parent_token->Release();
        (void)release_parent;
      }
      return std::nullopt;
    }
    fidl::SyncClient<fuchsia_sysmem2::BufferCollectionToken> child0(std::move(children->at(0)));
    // Releasing the unconstrained second group child gives sysmem a GPU-only fallback branch if the
    // display constraints on child0 are rejected (DESIGN.md#gpu-readable).
    fidl::SyncClient<fuchsia_sysmem2::BufferCollectionToken> empty_child(
        std::move(children->at(1)));
    auto release_empty = empty_child->Release();
    (void)release_empty;

    auto display_check_token = DuplicateTokenSync(child0, collection_id);
    if (!display_check_token.has_value()) {
      auto release_child0 = child0->Release();
      (void)release_child0;
      auto release_parent = parent_token->Release();
      (void)release_parent;
      return std::nullopt;
    }
    parts.display_participant = BindEmptyConstraintsParticipant(
        sysmem_allocator, std::move(*display_check_token), main_dispatcher_, collection_id);
    if (!parts.display_participant.is_valid()) {
      auto release_child0 = child0->Release();
      (void)release_child0;
      auto release_parent = parent_token->Release();
      (void)release_parent;
      return std::nullopt;
    }
    parts.display_token = child0.TakeClientEnd();
  }

  parts.allocation_participant = BindEmptyConstraintsParticipant(
      sysmem_allocator, parent_token.TakeClientEnd(), main_dispatcher_, collection_id);
  if (!parts.allocation_participant.is_valid()) {
    parts.Abandon();
    return std::nullopt;
  }
  return parts;
}

void ImageImporterImpl::FinishCoordinatorCollectionImportOnMain(
    allocation::GlobalBufferCollectionId collection_id,
    display::WireBufferCollectionId wire_collection_id, zx::result<> import_res) {
  FX_CHECK(utils::IsOnMainThread());
  if (stopping_.load(std::memory_order_relaxed) || !coordinator_) {
    return;
  }
  auto it = pending_registrations_.find(collection_id);
  if (it == pending_registrations_.end()) {
    return;
  }
  it->second->coordinator_import_in_flight = false;
  if (import_res.is_error()) {
    FinishCollectionRegistrationOnMain(collection_id, /*coordinator_succeeded=*/false);
    return;
  }
  it->second->coordinator_imported_collection = true;
  if (!it->second->gpu_collection ||
      (!config_.display_reply_thread &&
       watch_state_->channel_closed.load(std::memory_order_relaxed))) {
    FinishCollectionRegistrationOnMain(collection_id, /*coordinator_succeeded=*/false);
    return;
  }
  const display::WireImageBufferUsage usage{
      .tiling_type = fuchsia_hardware_display_types::wire::kImageTilingTypeLinear,
  };
  coordinator_->SetBufferCollectionConstraints(
      wire_collection_id, usage,
      [weak_this = weak_from_this(), gate = main_dispatcher_gate_,
       collection_id](zx::result<> constraints_res) {
        std::scoped_lock lock(gate->mutex);
        if (gate->dispatcher != nullptr) {
          async::PostTask(gate->dispatcher, [weak_this, collection_id, constraints_res]() {
            if (auto inner = weak_this.lock()) {
              inner->FinishCollectionRegistrationOnMain(collection_id, constraints_res.is_ok());
            }
          });
        }
      });
}

fpromise::promise<> ImageImporterImpl::ImportBufferCollection(
    allocation::GlobalBufferCollectionId collection_id,
    fidl::WireClient<fuchsia_sysmem2::Allocator>& sysmem_allocator,
    fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token,
    allocation::BufferCollectionUsage usage, std::optional<fuchsia_math::SizeU> /*size*/) {
  FX_CHECK(utils::IsOnMainThread());
  FX_CHECK(usage == allocation::BufferCollectionUsage::kClientImage);
  TRACE_DURATION("gfx", "ImageImporterImpl::ImportBufferCollection", "collection_id",
                 collection_id);

  if (!token.is_valid()) {
    return fpromise::make_error_promise();
  }
  if (stopping_.load(std::memory_order_relaxed)) {
    {
      TRACE_DURATION("gfx", "fuchsia.sysmem2.BufferCollectionToken::Release", "collection_id",
                     collection_id);
      auto release_token = fidl::WireCall(token)->Release();
      (void)release_token;
    }
    return fpromise::make_error_promise();
  }

  bool already_registered = false;
  {
    std::scoped_lock lock(collections_mutex_);
    already_registered =
        collections_.contains(collection_id) || pending_registrations_.contains(collection_id);
  }
  if (already_registered) {
    FX_LOGS(ERROR) << "Buffer collection " << collection_id << " is already registered";
    {
      TRACE_DURATION("gfx", "fuchsia.sysmem2.BufferCollectionToken::Release", "collection_id",
                     collection_id);
      auto release_token = fidl::WireCall(token)->Release();
      (void)release_token;
    }
    return fpromise::make_error_promise();
  }

  fidl::SyncClient<fuchsia_sysmem2::BufferCollectionToken> parent_token(std::move(token));
  // TODO(https://fxbug.dev/583950650): Avoid blocking main on synchronous sysmem
  // round trips here and in `SetupSysmemParticipants()`.
  auto gpu_token = DuplicateTokenSync(parent_token, collection_id);
  if (!gpu_token.has_value()) {
    auto release_parent = parent_token->Release();
    (void)release_parent;
    return fpromise::make_error_promise();
  }

  auto participants = SetupSysmemParticipants(collection_id, sysmem_allocator, parent_token);
  if (!participants.has_value()) {
    auto release_gpu = fidl::WireCall(*gpu_token)->Release();
    (void)release_gpu;
    return fpromise::make_error_promise();
  }

  if (!config_.enable_direct_to_display) {
    auto gpu_res = renderer_->CreateGpuCollection(collection_id, std::move(*gpu_token));
    if (gpu_res.is_error()) {
      participants->Abandon();
      return fpromise::make_error_promise();
    }
    auto collection = std::make_shared<ImportedCollection>(collection_id);
    collection->gpu_collection_ = std::move(gpu_res.value());
    collection->allocation_participant_ = std::move(participants->allocation_participant);
    collection->display_reply_received_ = true;
    collection->display_won_ = false;
    {
      std::scoped_lock lock(collections_mutex_);
      collections_.emplace(collection_id, collection);
    }
    StartCollectionAllocationWaits(collection);
    return fpromise::make_ok_promise();
  }

  if (!coordinator_ || (!config_.display_reply_thread &&
                        watch_state_->channel_closed.load(std::memory_order_relaxed))) {
    auto release_gpu = fidl::WireCall(*gpu_token)->Release();
    (void)release_gpu;
    participants->Abandon();
    return fpromise::make_error_promise();
  }

  fpromise::bridge<> bridge;
  auto pending = std::make_unique<PendingRegistration>();
  pending->allocation_participant = std::move(participants->allocation_participant);
  pending->display_participant = std::move(participants->display_participant);
  pending->completer = std::move(bridge.completer);
  pending->coordinator_import_in_flight = true;

  const display::WireBufferCollectionId wire_collection_id =
      display::ToDisplayFidlBufferCollectionId(collection_id);
  coordinator_->ImportBufferCollection(
      wire_collection_id, std::move(participants->display_token),
      [weak_this = weak_from_this(), gate = main_dispatcher_gate_, collection_id,
       wire_collection_id](zx::result<> import_res) {
        std::scoped_lock lock(gate->mutex);
        if (gate->dispatcher != nullptr) {
          async::PostTask(gate->dispatcher,
                          [weak_this, collection_id, wire_collection_id, import_res]() {
                            if (auto self = weak_this.lock()) {
                              self->FinishCoordinatorCollectionImportOnMain(
                                  collection_id, wire_collection_id, import_res);
                            }
                          });
        }
      });

  auto gpu_res = renderer_->CreateGpuCollection(collection_id, std::move(*gpu_token));
  if (gpu_res.is_error()) {
    ReleaseSysmemClient(pending->allocation_participant);
    ReleaseSysmemClient(pending->display_participant);
    pending_registrations_.emplace(collection_id, std::move(pending));
    return bridge.consumer.promise();
  }

  pending->gpu_collection = std::move(gpu_res.value());
  pending_registrations_.emplace(collection_id, std::move(pending));
  return bridge.consumer.promise();
}

void ImageImporterImpl::AbandonRegistration(allocation::GlobalBufferCollectionId collection_id,
                                            PendingRegistration& pending) {
  ReleaseSysmemClient(pending.allocation_participant);
  ReleaseSysmemClient(pending.display_participant);
  pending.gpu_collection.reset();
  // FIDL message ordering over the coordinator channel is strictly FIFO: the display coordinator
  // driver is guaranteed to receive `ImportBufferCollection()` first and
  // `ReleaseBufferCollection()` second. In the driver, `ReleaseBufferCollection()` cleans up the
  // imported collection (or safely no-ops if import failed), guaranteeing zero resource leaks even
  // when `StopOnMain()` runs mid-flight.
  if ((pending.coordinator_imported_collection || pending.coordinator_import_in_flight) &&
      coordinator_ &&
      !(!config_.display_reply_thread &&
        watch_state_->channel_closed.load(std::memory_order_relaxed))) {
    coordinator_->ReleaseBufferCollection(display::ToDisplayFidlBufferCollectionId(collection_id));
  }
  if (pending.completer) {
    pending.completer.complete_error();
  }
}

void ImageImporterImpl::FinishCollectionRegistrationOnMain(
    allocation::GlobalBufferCollectionId collection_id, bool coordinator_succeeded) {
  FX_CHECK(utils::IsOnMainThread());
  auto it = pending_registrations_.find(collection_id);
  if (it == pending_registrations_.end()) {
    return;
  }
  std::unique_ptr<PendingRegistration> pending = std::move(it->second);
  pending_registrations_.erase(it);

  // If `CreateGpuCollection()` failed earlier (`!pending->gpu_collection`) or
  // the coordinator half failed, clean up and reject registration.
  if (!coordinator_succeeded || !pending->gpu_collection ||
      stopping_.load(std::memory_order_relaxed)) {
    AbandonRegistration(collection_id, *pending);
    return;
  }

  auto collection = std::make_shared<ImportedCollection>(collection_id);
  collection->gpu_collection_ = std::move(pending->gpu_collection);
  collection->allocation_participant_ = std::move(pending->allocation_participant);
  collection->display_participant_ = std::move(pending->display_participant);
  collection->released_by_allocator_ = pending->released_by_allocator;
  if (collection->released_by_allocator_) {
    TearDown(*collection);
    if (pending->completer) {
      pending->completer.complete_error();
    }
    return;
  }
  {
    std::scoped_lock lock(collections_mutex_);
    collections_.emplace(collection_id, collection);
  }
  StartCollectionAllocationWaits(collection);
  if (pending->completer) {
    pending->completer.complete_ok();
  }
}

void ImageImporterImpl::StartCollectionAllocationWaits(
    const std::shared_ptr<ImportedCollection>& collection) {
  FX_CHECK(utils::IsOnMainThread());
  const auto collection_id = collection->id();

  if (collection->allocation_participant_.is_valid()) {
    TRACE_DURATION("gfx", "fuchsia.sysmem2.BufferCollection::WaitForAllBuffersAllocated",
                   "collection_id", collection_id);
    const trace_flow_id_t flow_id = TRACE_NONCE();
    TRACE_FLOW_BEGIN("gfx", "sysmem::WaitForAllBuffersAllocated[main]", flow_id);
    collection->allocation_participant_->WaitForAllBuffersAllocated().Then(
        [weak_this = weak_from_this(), collection, flow_id](
            fidl::Result<fuchsia_sysmem2::BufferCollection::WaitForAllBuffersAllocated>& result) {
          TRACE_DURATION("gfx",
                         "fuchsia.sysmem2.BufferCollection::WaitForAllBuffersAllocated[reply]",
                         "collection_id", collection->id());
          TRACE_FLOW_END("gfx", "sysmem::WaitForAllBuffersAllocated[main]", flow_id);
          auto self = weak_this.lock();
          if (!self || self->stopping_.load(std::memory_order_relaxed)) {
            return;
          }
          collection->allocation_reply_received_ = true;
          if (result.is_ok() && result->buffer_collection_info().has_value()) {
            collection->allocation_succeeded_ = true;
            const auto& info = result->buffer_collection_info().value();
            if (info.buffers().has_value()) {
              collection->buffer_count_ = static_cast<uint32_t>(info.buffers()->size());
            }
            if (info.settings().has_value()) {
              const auto& settings = info.settings().value();
              if (settings.buffer_settings().has_value() &&
                  settings.buffer_settings()->size_bytes().has_value()) {
                collection->per_buffer_bytes_ = ImportedCollection::VmoBytesPerBuffer(
                    settings.buffer_settings()->size_bytes().value());
              }
              if (settings.image_format_constraints().has_value()) {
                const auto& ic = settings.image_format_constraints().value();
                if (ic.min_size().has_value()) {
                  collection->min_size_ = ic.min_size().value();
                }
                if (ic.max_size().has_value()) {
                  collection->max_size_ = ic.max_size().value();
                }
              }
            }
          } else {
            collection->allocation_succeeded_ = false;
          }
          // Send one-way `Release()` immediately so closing this participant
          // never fails a surviving client collection (DESIGN.md#cleanup-on-main).
          ReleaseSysmemClient(collection->allocation_participant_);
          self->MaybeFinishCollectionAllocation(collection);
        });
  }

  if (collection->display_participant_.is_valid()) {
    TRACE_DURATION("gfx", "fuchsia.sysmem2.BufferCollection::WaitForAllBuffersAllocated",
                   "collection_id", collection_id);
    const trace_flow_id_t flow_id = TRACE_NONCE();
    TRACE_FLOW_BEGIN("gfx", "sysmem::WaitForAllBuffersAllocated[display]", flow_id);
    collection->display_participant_->WaitForAllBuffersAllocated().Then(
        [weak_this = weak_from_this(), collection, flow_id](
            fidl::Result<fuchsia_sysmem2::BufferCollection::WaitForAllBuffersAllocated>& result) {
          TRACE_DURATION("gfx",
                         "fuchsia.sysmem2.BufferCollection::WaitForAllBuffersAllocated[reply]",
                         "collection_id", collection->id());
          TRACE_FLOW_END("gfx", "sysmem::WaitForAllBuffersAllocated[display]", flow_id);
          auto self = weak_this.lock();
          if (!self || self->stopping_.load(std::memory_order_relaxed)) {
            return;
          }
          collection->display_reply_received_ = true;
          collection->display_won_ = result.is_ok();
          if (result.is_ok() && result->buffer_collection_info().has_value() &&
              result->buffer_collection_info()->settings().has_value() &&
              result->buffer_collection_info()
                  ->settings()
                  ->image_format_constraints()
                  .has_value()) {
            const auto& ic =
                result->buffer_collection_info()->settings()->image_format_constraints().value();
            if (ic.pixel_format_modifier().has_value()) {
              collection->tiling_type_ = display::BufferCollectionPixelFormatToImageTilingType(
                  ic.pixel_format_modifier().value());
            }
          }
          ReleaseSysmemClient(collection->display_participant_);
          self->MaybeFinishCollectionAllocation(collection);
        });
  }
}

// static
std::optional<ImportImageError> ImageImporterImpl::ValidateImageForCollection(
    const ImportedImage& image, const ImportedCollection& collection) {
  const auto& meta = image.metadata();
  if (meta.vmo_index >= collection.buffer_count_) {
    return ImportImageError::kInvalidVmoIndex;
  }
  if (meta.width == 0 || meta.height == 0 || meta.width > INT32_MAX || meta.height > INT32_MAX ||
      meta.width < collection.min_size_.width() || meta.height < collection.min_size_.height() ||
      meta.width > collection.max_size_.width() || meta.height > collection.max_size_.height()) {
    return ImportImageError::kImageTooLarge;
  }
  return std::nullopt;
}

// static
bool ImageImporterImpl::MarkImageAllocated(ImportedImage& image, bool display_capable) {
  auto swapped = Transition(image, [display_capable](ImportWord& w) {
    if (w.released) {
      return false;
    }
    w.allocated = true;
    if (!display_capable) {
      w.display = ImportState::kUnsupported;
    } else if (w.display == ImportState::kNotStarted) {
      w.display = ImportState::kQueued;
    }
    if (w.gpu == ImportState::kNotStarted) {
      w.gpu = ImportState::kQueued;
    }
    return true;
  });
  return swapped.has_value();
}

void ImageImporterImpl::FailImage(ImportedImage& image, ImportImageError code) {
  auto on_failure = std::move(image.on_failure_);
  auto reply_disp = image.reply_dispatcher_.lock();
  image.reply_dispatcher_.reset();
  image.gpu_error_ = code;
  auto swapped = Transition(image, [](ImportWord& w) {
    if (w.released) {
      return false;
    }
    w.display = ImportState::kFailed;
    w.gpu = ImportState::kFailed;
    return true;
  });
  if (swapped.has_value()) {
    inspect_image_imports_rejected_.Add(1);
    if (on_failure && reply_disp) {
      async::PostTask(reply_disp->dispatcher(), [cb = std::move(on_failure), id = image.id(),
                                                 code]() mutable { cb(id, code); });
    }
    ResolvePendingEntries(image);
  }
}

bool ImageImporterImpl::EnqueueLocked(std::span<ImageToEnqueue> images) {
  if (stopping_.load(std::memory_order_relaxed)) {
    return false;
  }
  PruneStaleQueueEntriesLocked();
  for (ImageToEnqueue& item : images) {
    FX_DCHECK(item.image);
    // `SafeCollectionBufferBytes()` is inlined inside `TRACE_INSTANT` rather than stored in a
    // local variable so that it is only evaluated when `"gfx"` tracing is enabled.
    TRACE_INSTANT("gfx", "ImportedImage::Enqueue", TRACE_SCOPE_THREAD, "image_id",
                  item.image->id().value(), "collection_id", item.image->metadata().collection_id,
                  "bytes", SafeCollectionBufferBytes(item.image->collection().get()));
    TRACE_FLOW_STEP("gfx", "flatland::ImportedImage", ImageFlowId(item.image->id()));
    if (item.display_capable) {
      display_queue_.push_back(item.image);
    }
    gpu_queue_.push_back(std::move(item.image));
  }
  // In case any items were pruned/enqueued above.
  EmitQueueTraceCountersLocked();

  return RefreshGpuCanStartLocked();
}

bool ImageImporterImpl::RefreshGpuCanStartLocked() {
  const bool gpu_can_start = CanStartKindNextLocked(JobKind::kGpu);
  gpu_jobs_deferred_ = !gpu_can_start;
  return gpu_can_start;
}

void ImageImporterImpl::WakeGpuRunner() {
  if (config_.worker_pool_size > 0) {
    gpu_queue_nonempty_.notify_all();
  } else {
    ScheduleDispatcher();
  }
}

bool ImageImporterImpl::SignalPendingEntriesLocked(ImportedImage& image, zx::time now) {
  const ImportState disp = image.display_state();
  const bool image_is_gpu_only = disp == ImportState::kUnsupported || disp == ImportState::kFailed;
  bool newly_hinted = false;
  for (auto& entry : image.pending_) {
    FX_CHECK(entry.signal->countdown > 0);
    if (image_is_gpu_only) {
      entry.signal->names_gpu_only = true;
    }
    if (--entry.signal->countdown == 0) {
      const uint64_t delay_us = ToMicrosClamped(now - entry.signal->created_at);
      if (entry.signal->has_wait_for_imports) {
        inspect_present_delay_wait_for_imports_us_.Insert(delay_us);
      } else {
        inspect_present_delay_optimistic_us_.Insert(delay_us);
      }
      if (entry.signal->names_gpu_only && config_.enable_direct_to_display &&
          !present_named_gpu_only_.exchange(true, std::memory_order_relaxed)) {
        newly_hinted = true;
      }
      entry.signal->event.signal(0, ZX_EVENT_SIGNALED);
    }
  }
  image.pending_.clear();
  return newly_hinted;
}

void ImageImporterImpl::MaybeFinishCollectionAllocation(
    const std::shared_ptr<ImportedCollection>& collection) {
  FX_CHECK(utils::IsOnMainThread());
  if (!collection->allocation_reply_received_ || !collection->display_reply_received_) {
    return;
  }
  if (collection->state() != ImportedCollection::State::kRegistered) {
    return;
  }

  const ImportedCollection::State final_state =
      !collection->allocation_succeeded_
          ? ImportedCollection::State::kFailed
          : (collection->display_won_ ? ImportedCollection::State::kDisplayCapable
                                      : ImportedCollection::State::kGpuOnly);
  if (final_state == ImportedCollection::State::kFailed) {
    inspect_collection_allocations_failed_.Add(1);
  }

  std::vector<std::shared_ptr<ImportedImage>> waiting;
  {
    std::scoped_lock lock(collections_mutex_);
    if (collection->state() != ImportedCollection::State::kRegistered) {
      return;
    }
    collection->state_.store(final_state, std::memory_order_release);
    waiting = std::move(collection->waiting_for_allocation_);
    collection->waiting_for_allocation_.clear();
  }

  bool any_display_queued = false;
  std::vector<ImageToEnqueue> to_enqueue;
  to_enqueue.reserve(waiting.size());

  for (const auto& img : waiting) {
    if (img->released()) {
      continue;
    }
    if (final_state == ImportedCollection::State::kFailed) {
      FailImage(*img, ImportImageError::kCollectionFailed);
      continue;
    }

    if (const auto error = ValidateImageForCollection(*img, *collection); error.has_value()) {
      FailImage(*img, *error);
      continue;
    }

    const bool display_capable = (final_state == ImportedCollection::State::kDisplayCapable);
    if (!MarkImageAllocated(*img, display_capable)) {
      continue;
    }

    to_enqueue.push_back(ImageToEnqueue{
        .image = img,
        .display_capable = display_capable,
    });
    any_display_queued = any_display_queued || display_capable;
  }

  bool wake_gpu = false;
  if (!to_enqueue.empty()) {
    std::scoped_lock lock(queue_mutex_);
    wake_gpu = EnqueueLocked(to_enqueue);
  }

  // Resolve pending entries after queuing so `kOptimistic` waiters unblock.
  for (const auto& img : waiting) {
    if (!img->released() && img->word().allocated) {
      ResolvePendingEntries(*img);
    }
  }

  if (wake_gpu) {
    WakeGpuRunner();
  }
  if (any_display_queued) {
    ScheduleDispatcher();
  }
}

std::shared_ptr<ImportedImage> ImageImporterImpl::ImportImage(
    const allocation::ImageMetadata& metadata, ImageReadinessPolicy policy,
    const std::shared_ptr<utils::DispatcherHolder>& reply_dispatcher, FailureCallback on_failure) {
  FX_CHECK(metadata.identifier == allocation::kInvalidImageId);

  allocation::ImageMetadata assigned = metadata;
  assigned.identifier = allocation::GenerateUniqueImageId();

  TRACE_DURATION("gfx", "ImageImporterImpl::ImportImage", "image_id", assigned.identifier.value(),
                 "collection_id", assigned.collection_id);

  auto image = std::make_shared<ImportedImage>(assigned, policy, std::move(reply_dispatcher),
                                               std::move(on_failure));

  bool allocated = false;
  bool display_capable = false;
  bool wake_worker = false;
  bool schedule_main = false;
  std::optional<ImportImageError> immediate_error;
  FailureCallback failure_cb;
  std::shared_ptr<utils::DispatcherHolder> failure_disp;

  {
    std::scoped_lock coll_lock(collections_mutex_);
    if (stopping_.load(std::memory_order_relaxed)) {
      immediate_error = ImportImageError::kStopped;
    } else {
      live_images_by_id_.emplace(image->id(), image);
      if (assigned.collection_id == allocation::kInvalidId) {
        immediate_error = ImportImageError::kUnknownCollection;
      } else {
        auto it = collections_.find(assigned.collection_id);
        if (it == collections_.end()) {
          immediate_error = ImportImageError::kUnknownCollection;
        } else {
          const auto& coll = it->second;
          image->collection_ = coll;
          ++coll->live_images_;

          const auto coll_state = coll->state();
          switch (coll_state) {
            case ImportedCollection::State::kRegistered:
              coll->waiting_for_allocation_.push_back(image);
              break;
            case ImportedCollection::State::kFailed:
            case ImportedCollection::State::kReleased:
              immediate_error = ImportImageError::kCollectionFailed;
              break;
            case ImportedCollection::State::kDisplayCapable:
            case ImportedCollection::State::kGpuOnly:
              // The collection already finished allocating, so handle `image` the
              // way `MaybeFinishCollectionAllocation()` handles images that
              // arrived before allocation finished: validate it, then mark it
              // allocated with its imports `kQueued`. It is pushed onto the
              // queues below, after `collections_mutex_` is released.
              immediate_error = ValidateImageForCollection(*image, *coll);
              if (!immediate_error.has_value()) {
                display_capable = (coll_state == ImportedCollection::State::kDisplayCapable);
                // `image` is not yet visible to any other thread, so it cannot be released.
                allocated = MarkImageAllocated(*image, display_capable);
              }
              break;
          }
        }
      }
    }

    if (immediate_error.has_value()) {
      failure_cb = std::move(image->on_failure_);
      failure_disp = image->reply_dispatcher_.lock();
      image->reply_dispatcher_.reset();
      image->gpu_error_ = immediate_error;
      Transition(*image,
                 [stopped = (*immediate_error == ImportImageError::kStopped)](ImportWord& w) {
                   w.display = ImportState::kFailed;
                   w.gpu = ImportState::kFailed;
                   if (stopped) {
                     w.released = true;
                   }
                   return true;
                 });
    }
  }

  if (immediate_error.has_value()) {
    if (*immediate_error != ImportImageError::kStopped) {
      TRACE_FLOW_BEGIN("gfx", "flatland::ImportedImage", ImageFlowId(assigned.identifier));
      inspect_image_imports_rejected_.Add(1);
      if (failure_cb && failure_disp) {
        const ImportImageError code = *immediate_error;
        async::PostTask(failure_disp->dispatcher(),
                        [cb = std::move(failure_cb), id = assigned.identifier, code]() mutable {
                          cb(id, code);
                        });
      }
    }
    return image;
  }

  TRACE_FLOW_BEGIN("gfx", "flatland::ImportedImage", ImageFlowId(assigned.identifier));

  if (allocated) {
    std::scoped_lock queue_lock(queue_mutex_);
    ImageToEnqueue item{
        .image = image,
        .display_capable = display_capable,
    };
    wake_worker =
        EnqueueLocked(std::span<ImageToEnqueue>(&item, 1)) && config_.worker_pool_size > 0;
    // Avoid waking main on session `ImportImage()` calls during active frames
    // under `kWhenIdle`; only schedule main immediately when stamp-0 jobs are
    // already eligible (`kWhenFree`, or `idle_` set).
    if (!stopping_.load(std::memory_order_relaxed) && NeverShownEligibleLocked()) {
      schedule_main = (display_capable && CanStartKindNextLocked(JobKind::kDisplay)) ||
                      (config_.worker_pool_size == 0 && !gpu_jobs_deferred_);
    }
  }

  if (wake_worker) {
    // `ImportImage()` enqueues at most one GPU job, so waking one worker suffices.
    gpu_queue_nonempty_.notify_one();
  }
  if (schedule_main) {
    ScheduleDispatcher();
  }
  return image;
}

void ImageImporterImpl::ReleaseImage(const std::shared_ptr<ImportedImage>& image) {
  if (!image) {
    return;
  }
  // Capture `id` before the CAS because another thread finishing an in-flight
  // import immediately after our CAS could post cleanup and destroy `*image` on
  // main before `ReleaseImage()` returns (DESIGN.md#owner-acts).
  const allocation::GlobalImageId id = image->id();
  auto swapped = Transition(*image, [](ImportWord& w) {
    if (w.released) {
      return false;
    }
    w.released = true;
    return true;
  });
  if (!swapped.has_value()) {
    // Image was already released.
    return;
  }

  const ImportWord next = swapped->second;
  if (next.display != ImportState::kImporting && next.gpu != ImportState::kImporting) {
    if (utils::IsOnMainThread()) {
      CleanUpById(id);
    } else {
      PostCleanUp(id);
    }
  }
}

zx::event ImageImporterImpl::SignalWhenReady(std::span<ImportedImage* const> images) {
  TRACE_DURATION_BEGIN("gfx", "ImageImporterImpl::SignalWhenReady", "count", images.size());
  uint32_t num_unsatisfied = 0;
  auto defer_trace_end = fit::defer([&] {
    TRACE_DURATION_END("gfx", "ImageImporterImpl::SignalWhenReady", "unsatisfied", num_unsatisfied);
  });
  if (images.empty()) {
    return {};
  }
  utils::PmrStackArena<512> arena;
  std::pmr::vector<ImportedImage*> unique(arena.resource());
  DeduplicateImages(images, unique);
  if (unique.empty()) {
    return {};
  }

  const uint64_t next_stamp =
      (frames_composed_.load(std::memory_order_relaxed) + 1) | ImportedImage::kNamedByPresent;

  bool any_gpu_only = false;
  bool any_queued = false;
  bool all_satisfied = true;

  for (ImportedImage* img : unique) {
    const ImportWord w = img->word();
    const bool already_terminal =
        IsTerminalImportState(w.gpu) &&
        (!config_.enable_direct_to_display || IsTerminalImportState(w.display));
    if (!already_terminal) {
      img->frame_stamp_.store(next_stamp, std::memory_order_relaxed);
    }
    if (w.display == ImportState::kUnsupported || w.display == ImportState::kFailed) {
      any_gpu_only = true;
    }
    if (w.display == ImportState::kQueued || w.gpu == ImportState::kQueued) {
      any_queued = true;
    }
    if (!SatisfiesReadiness(w, img->policy())) {
      all_satisfied = false;
    }
  }

  bool newly_hinted_gpu_only = false;
  if (any_gpu_only && config_.enable_direct_to_display) {
    if (all_satisfied) {
      newly_hinted_gpu_only = !present_named_gpu_only_.exchange(true, std::memory_order_relaxed);
    }
  }

  bool wake_gpu = false;
  if (any_queued || newly_hinted_gpu_only) {
    std::scoped_lock lock(queue_mutex_);
    const bool was_deferred = gpu_jobs_deferred_;
    // Stamping `unique` into class 1 or setting `present_named_gpu_only_` may
    // make an already-queued GPU job eligible.
    wake_gpu = RefreshGpuCanStartLocked() && (any_queued || was_deferred);
  }

  zx::event ready_event;
  if (!all_satisfied) {
    TRACE_DURATION("gfx", "ImageImporterImpl::SignalWhenReady[handle_unsatisfied]");
    zx::event local_event;
    const zx_status_t ev_status = zx::event::create(0, &local_event);
    FX_CHECK(ev_status == ZX_OK);
    zx::event caller_event;
    const zx_status_t dup_status = local_event.duplicate(ZX_RIGHT_SAME_RIGHTS, &caller_event);
    FX_CHECK(dup_status == ZX_OK);

    auto signal = std::make_shared<PendingSignal>();
    signal->event = std::move(local_event);
    signal->created_at = zx::clock::get_monotonic();

    bool hint_latched_under_pending = false;
    {
      std::scoped_lock lock(pending_signal_mutex_);
      if (any_gpu_only) {
        signal->names_gpu_only = true;
      }
      for (ImportedImage* img : unique) {
        const ImportWord w = img->word();
        if (w.display == ImportState::kUnsupported || w.display == ImportState::kFailed) {
          signal->names_gpu_only = true;
        }
        if (!SatisfiesReadiness(w, img->policy())) {
          ++num_unsatisfied;
          ++signal->countdown;
          if (img->policy() == ImageReadinessPolicy::kWaitForImports) {
            signal->has_wait_for_imports = true;
          }
          img->pending_.push_back(PendingEntry{.signal = signal});
        }
      }
      if (signal->countdown > 0) {
        ready_event = std::move(caller_event);
      } else if (signal->names_gpu_only && config_.enable_direct_to_display) {
        hint_latched_under_pending =
            !present_named_gpu_only_.exchange(true, std::memory_order_relaxed);
      }
    }
    if (hint_latched_under_pending) {
      std::scoped_lock lock(queue_mutex_);
      const bool was_deferred = gpu_jobs_deferred_;
      if (RefreshGpuCanStartLocked() && was_deferred) {
        wake_gpu = true;
      }
    }
  }

  if (wake_gpu) {
    WakeGpuRunner();
  }
  if (any_queued) {
    ScheduleDispatcher();
  }
  return ready_event;
}

void ImageImporterImpl::ResolvePendingEntries(ImportedImage& image) {
  const zx::time now = zx::clock::get_monotonic();
  bool newly_hinted = false;
  {
    std::scoped_lock lock(pending_signal_mutex_);
    if (!image.pending_.empty() && SatisfiesReadiness(image.word(), image.policy())) {
      newly_hinted = SignalPendingEntriesLocked(image, now);
    }
  }
  if (newly_hinted && !stopping_.load(std::memory_order_relaxed)) {
    bool wake_gpu = false;
    {
      std::scoped_lock lock(queue_mutex_);
      const bool was_deferred = gpu_jobs_deferred_;
      wake_gpu = RefreshGpuCanStartLocked() && was_deferred;
    }
    if (wake_gpu) {
      WakeGpuRunner();
    } else {
      ScheduleDispatcher();
    }
  }
}

FrameOutcome ImageImporterImpl::CheckDisplayStates(std::span<ImportedImage* const> images) const {
  FX_DCHECK(utils::IsOnMainThread());
  DCheckUniqueUnreleasedImages(images);
  bool any_failed = false;
  for (ImportedImage* img : images) {
    const ImportState disp = img->display_state();
    if (disp == ImportState::kUnsupported) {
      return FrameOutcome::kGpuOnlyImage;
    }
    // Check `ImportState::kReleased` as release-build defense-in-depth alongside
    // `DCheckUniqueUnreleasedImages()`.
    if (disp == ImportState::kFailed || disp == ImportState::kReleased || !img->collection() ||
        img->collection()->state() != ImportedCollection::State::kDisplayCapable) {
      if (img->collection() && img->collection()->state() == ImportedCollection::State::kGpuOnly) {
        return FrameOutcome::kGpuOnlyImage;
      }
      any_failed = true;
    }
  }
  return any_failed ? FrameOutcome::kDisplayImportFailed : FrameOutcome::kDirect;
}

FrameOutcome ImageImporterImpl::PrepareForDirectToDisplay(std::span<ImportedImage* const> images) {
  FX_DCHECK(utils::IsOnMainThread());
  DCheckUniqueUnreleasedImages(images);

  waiters_scratch_.clear();
  waiters_scratch_.reserve(images.size());

  // Start every missing display import up front before waiting for any reply
  // (DESIGN.md#dispatcher).
  for (ImportedImage* img : images) {
    const ImportWord w = img->word();
    if (IsTerminalImportState(w.display)) {
      continue;
    }
    auto swapped = Transition(*img, [](ImportWord& next) {
      if (next.released) {
        return false;
      }
      if (next.display != ImportState::kQueued) {
        return false;
      }
      next.display = ImportState::kImporting;
      return true;
    });
    if (swapped.has_value()) {
      SendDisplayImport(img->shared_from_this());
      waiters_scratch_.push_back(WaitItem{.image = img, .sent_by_frame = true});
    } else if (img->display_state() == ImportState::kImporting) {
      waiters_scratch_.push_back(WaitItem{.image = img, .sent_by_frame = false});
    }
  }

  for (const WaitItem& item : waiters_scratch_) {
    if (item.image->display_state() != ImportState::kImporting) {
      continue;
    }

    TRACE_DURATION("gfx", "ImageImporterImpl::PrepareForDirectToDisplay[wait]", "image_id",
                   item.image->id().value(), "sent_by_frame", item.sent_by_frame);
    const zx::time start = zx::clock::get_monotonic();
    if (!config_.display_reply_thread) {
      watch_state_->readable_wait.Cancel();
      while (item.image->display_state() == ImportState::kImporting) {
        const zx_status_t status = reply_loop_.Run(zx::time::infinite(), /*once=*/true);
        FX_CHECK(status == ZX_OK)
            << "reply_loop must remain running during PrepareForDirectToDisplay: "
            << zx_status_get_string(status);
      }
      if (!watch_state_->channel_closed.load(std::memory_order_relaxed) &&
          !stopping_.load(std::memory_order_relaxed) && coordinator_ &&
          coordinator_->channel_handle() != ZX_HANDLE_INVALID) {
        const zx_status_t rearm = watch_state_->readable_wait.Begin(main_dispatcher_);
        if (rearm != ZX_OK && rearm != ZX_ERR_BAD_STATE) {
          watch_state_->channel_closed.store(true, std::memory_order_relaxed);
        }
      }
    } else {
      std::unique_lock<std::mutex> lock(pending_signal_mutex_);
      display_import_done_.wait(
          lock, [&] { return item.image->display_state() != ImportState::kImporting; });
    }
    const zx::duration waited = zx::clock::get_monotonic() - start;
    inspect_image_wait_display_us_.Insert(ToMicrosClamped(waited));
    if (item.sent_by_frame) {
      current_frame_display_sent_wait_ += waited;
    } else {
      current_frame_display_inflight_wait_ += waited;
    }
  }

  for (ImportedImage* img : images) {
    if (img->display_state() != ImportState::kReady) {
      return FrameOutcome::kDisplayImportFailed;
    }
  }
  return FrameOutcome::kDirect;
}

void ImageImporterImpl::PrepareForGpuComposition(std::span<ImportedImage* const> images) {
  FX_DCHECK(utils::IsOnMainThread());
  DCheckUniqueUnreleasedImages(images);

  uint32_t pending_count = 0;
  uint64_t first_waiting_id = 0;
  for (ImportedImage* img : images) {
    const ImportWord w = img->word();
    if (!IsTerminalImportState(w.gpu)) {
      if (pending_count == 0) {
        first_waiting_id = img->id().value();
      }
      ++pending_count;
    }
  }
  if (pending_count == 0) {
    return;
  }

  TRACE_DURATION("gfx", "ImageImporterImpl::PrepareForGpuComposition", "waiting_count",
                 pending_count, "first_image_id", first_waiting_id);
  const zx::time call_start = zx::clock::get_monotonic();

  for (ImportedImage* img : images) {
    const ImportWord w0 = img->word();
    if (IsTerminalImportState(w0.gpu)) {
      continue;
    }
    // `SignalWhenReady()` gates presents until `allocated` is set or GPU
    // reaches a terminal failure (DESIGN.md#readiness).
    FX_DCHECK(w0.allocated);

    TRACE_DURATION("gfx", "ImageImporterImpl::PrepareForGpuComposition[image]", "image_id",
                   img->id().value());
    const zx::time img_start = zx::clock::get_monotonic();
    // Locking `gpu_run_mutex_` lends main's deadline profile to any worker
    // currently executing this image's GPU import (DESIGN.md#lock-order).
    std::unique_lock<std::mutex> gpu_lock(img->gpu_run_mutex_);
    auto swapped = Transition(*img, [](ImportWord& w) {
      if (w.released || w.gpu != ImportState::kQueued) {
        return false;
      }
      w.gpu = ImportState::kImporting;
      return true;
    });
    if (swapped.has_value()) {
      ExecuteGpuImport(*img, gpu_lock);
    } else {
      gpu_lock.unlock();
    }

    const zx::duration img_wait = zx::clock::get_monotonic() - img_start;
    inspect_image_wait_gpu_us_.Insert(ToMicrosClamped(img_wait));
  }

  const zx::duration total_wait = zx::clock::get_monotonic() - call_start;
  current_frame_gpu_wait_ += total_wait;
  if (total_wait.to_nsecs() > 0) {
    inspect_prepare_for_gpu_ns_.Add(static_cast<uint64_t>(total_wait.to_nsecs()));
  }
}

void ImageImporterImpl::SendDisplayImport(std::shared_ptr<ImportedImage> image) {
  FX_DCHECK(utils::IsOnMainThread());
  FX_CHECK(image != nullptr);

  if (!coordinator_ || !image->collection() ||
      (!config_.display_reply_thread &&
       watch_state_->channel_closed.load(std::memory_order_relaxed))) {
    display_imports_in_flight_.fetch_add(1, std::memory_order_acq_rel);
    display_imports_outstanding_.fetch_add(1, std::memory_order_acq_rel);
    OnDisplayImportComplete(std::move(image), zx::clock::get_monotonic(),
                            zx::error(ZX_ERR_PEER_CLOSED));
    return;
  }

  const uint32_t in_flight = display_imports_in_flight_.fetch_add(1, std::memory_order_acq_rel) + 1;
  display_imports_outstanding_.fetch_add(1, std::memory_order_acq_rel);
  TRACE_COUNTER("gfx", "ImageImporter::DisplayInFlight", 0, "in_flight", in_flight);

  const types::Extent2 dims({
      .width = static_cast<int32_t>(image->metadata().width),
      .height = static_cast<int32_t>(image->metadata().height),
  });
  const display::WireBufferCollectionId wire_coll_id =
      display::ToDisplayFidlBufferCollectionId(image->collection()->id());
  const allocation::GlobalImageId image_id = image->id();
  const uint32_t vmo_index = image->metadata().vmo_index;
  const uint32_t tiling_type = image->collection()->tiling_type();
  const zx::time sent_at = zx::clock::get_monotonic();

  if (config_.display_reply_thread) {
    // Raw `this` is safe here: `StopOnMain()` waits until
    // `display_imports_outstanding_` reaches 0 before returning, so the owner
    // cannot destroy `this` before `OnDisplayImportComplete()` finishes its
    // final decrement and unlock (DESIGN.md#cleanup-on-main).
    coordinator_->ImportImage(dims, tiling_type, wire_coll_id, vmo_index, image_id,
                              [this, img = std::move(image), sent_at](zx::result<> result) mutable {
                                OnDisplayImportComplete(std::move(img), sent_at, result);
                              });
  } else {
    coordinator_->ImportImage(dims, tiling_type, wire_coll_id, vmo_index, image_id,
                              [weak_this = weak_from_this(), img = std::move(image),
                               sent_at](zx::result<> result) mutable {
                                if (auto self = weak_this.lock()) {
                                  self->OnDisplayImportComplete(std::move(img), sent_at, result);
                                }
                              });
  }
}

void ImageImporterImpl::OnDisplayImportComplete(std::shared_ptr<ImportedImage> image,
                                                zx::time sent_at, zx::result<> result) {
  {
    TRACE_DURATION("gfx", "ImageImporterImpl::OnDisplayImportComplete", "image_id",
                   image->id().value(), "succeeded", result.is_ok());
    const zx::time handler_now = zx::clock::get_monotonic();
    const bool succeeded = result.is_ok();
    if (succeeded) {
      image->display_ready_latency_ = handler_now - image->created();
    } else {
      inspect_display_imports_failed_.Add(1);
      FX_LOGS(WARNING) << "Display coordinator failed to import client image "
                       << image->id().value() << ": " << result.status_string();
    }

    const uint32_t prev_in_flight =
        display_imports_in_flight_.fetch_sub(1, std::memory_order_acq_rel);
    FX_CHECK(prev_in_flight > 0);
    TRACE_COUNTER("gfx", "ImageImporter::DisplayInFlight", 0, "in_flight", prev_in_flight - 1);

    std::optional<zx::time> arrival_time;
    if (!config_.display_reply_thread && pending_channel_readable_timestamp_.has_value() &&
        *pending_channel_readable_timestamp_ >= sent_at) {
      arrival_time = *pending_channel_readable_timestamp_;
      inspect_display_reply_handling_delay_us_.Insert(ToMicrosClamped(handler_now - *arrival_time));
    }

    bool wake_gpu = false;
    {
      std::scoped_lock qlock(queue_mutex_);
      if (succeeded) {
        const zx::time finish_time = arrival_time.has_value() ? *arrival_time : handler_now;
        display_import_duration_.InsertNewMeasurement(
            std::max(zx::duration{0}, finish_time - sent_at));
      }
      // `display_imports_in_flight_` has dropped, which may allow deferred GPU jobs to start.
      wake_gpu = RefreshGpuCanStartLocked();
    }

    auto swapped = Transition(*image, [succeeded](ImportWord& w) {
      if (w.display != ImportState::kImporting) {
        return false;
      }
      w.display = succeeded ? ImportState::kReady : ImportState::kFailed;
      return true;
    });

    if (succeeded && swapped.has_value()) {
      // `SafeCollectionBufferBytes()` is inlined inside `TRACE_INSTANT` rather than stored in a
      // local variable so that it is only evaluated when `"gfx"` tracing is enabled.
      TRACE_INSTANT("gfx", "ImportedImage::DisplayReady", TRACE_SCOPE_THREAD, "image_id",
                    image->id().value(), "collection_id", image->metadata().collection_id, "bytes",
                    SafeCollectionBufferBytes(image->collection().get()));
      TRACE_FLOW_STEP("gfx", "flatland::ImportedImage", ImageFlowId(image->id()));
    }

    ResolvePendingEntries(*image);

    if (wake_gpu) {
      WakeGpuRunner();
    }

    // Always post cleanup to `main_dispatcher_` rather than running inline,
    // because main may be pumping `reply_loop_` inside `RenderFrame()`
    // (DESIGN.md#cleanup-on-main).
    if (swapped.has_value() && swapped->second.released &&
        swapped->second.gpu != ImportState::kImporting) {
      PostCleanUp(image->id());
    }

    if (!stopping_.load(std::memory_order_relaxed)) {
      ScheduleDispatcher();
    }
  }

  // Finish all importer and `image` work first, then decrement
  // `display_imports_outstanding_` and notify `display_import_done_` under
  // `pending_signal_mutex_` as the final access to `this` so `StopOnMain()`
  // can safely return and let the owner destroy the importer on main
  // (DESIGN.md#cleanup-on-main).
  image.reset();
  std::function<void()> post_decrement_hook;
  {
    std::scoped_lock slock(pending_signal_mutex_);
    post_decrement_hook = post_display_decrement_hook_for_test_;
    display_imports_outstanding_.fetch_sub(1, std::memory_order_acq_rel);
    display_import_done_.notify_all();
  }
  if (post_decrement_hook) {
    post_decrement_hook();
  }
}

void ImageImporterImpl::OnReplyChannelReadable(async_dispatcher_t* dispatcher,
                                               async::Wait* /*wait*/, zx_status_t status,
                                               const zx_packet_signal_t* signal) {
  FX_DCHECK(utils::IsOnMainThread());
  ++reply_channel_readable_calls_for_test_;
  if (status != ZX_OK || stopping_.load(std::memory_order_relaxed)) {
    return;
  }

  if (signal != nullptr && signal->timestamp != 0) {
    pending_channel_readable_timestamp_ = zx::time(signal->timestamp);
  } else {
    pending_channel_readable_timestamp_.reset();
  }

  reply_loop_.RunUntilIdle();
  pending_channel_readable_timestamp_.reset();

  // `ZX_CHANNEL_PEER_CLOSED` stays asserted, so re-arming after it would fire
  // this handler forever. The `on_fidl_error()` callback sets `channel_closed`
  // for the same reason.
  if (signal != nullptr && (signal->observed & ZX_CHANNEL_PEER_CLOSED) != 0) {
    watch_state_->channel_closed.store(true, std::memory_order_relaxed);
  }

  if (!watch_state_->channel_closed.load(std::memory_order_relaxed) &&
      !stopping_.load(std::memory_order_relaxed)) {
    const zx_status_t rearm = watch_state_->readable_wait.Begin(dispatcher);
    if (rearm != ZX_OK && rearm != ZX_ERR_BAD_STATE) {
      watch_state_->channel_closed.store(true, std::memory_order_relaxed);
    }
  }
}

ImageImporterImpl::JobKind ImageImporterImpl::PredictedNextKind() const {
  if (!config_.enable_direct_to_display ||
      present_named_gpu_only_.load(std::memory_order_relaxed)) {
    return JobKind::kGpu;
  }
  return last_outcome_.load(std::memory_order_relaxed) == FrameOutcome::kDirect ? JobKind::kDisplay
                                                                                : JobKind::kGpu;
}

bool ImageImporterImpl::NeverShownEligibleLocked() const {
  return config_.never_shown_imports == NeverShownImports::kWhenFree ||
         (config_.never_shown_imports == NeverShownImports::kWhenIdle && idle_);
}

ImageImporterImpl::JobPriority ImageImporterImpl::ComputeJobPriority(const ImportedImage& image,
                                                                     JobKind kind,
                                                                     uint64_t frames_composed,
                                                                     JobKind predicted_kind) const {
  const uint64_t raw_stamp = image.frame_stamp();
  const bool named_by_present = (raw_stamp & ImportedImage::kNamedByPresent) != 0;
  const uint64_t frame_num = raw_stamp & ~ImportedImage::kNamedByPresent;
  const bool optimistic = (image.policy() == ImageReadinessPolicy::kOptimistic);

  JobClass cls = JobClass::kNeverNamed;
  if (raw_stamp != 0) {
    if (named_by_present) {
      if (!optimistic || (frame_num > frames_composed && kind == predicted_kind)) {
        cls = JobClass::kNamedByPresent;
      } else {
        cls = JobClass::kUsed;
      }
    } else {
      cls = JobClass::kUsed;
    }
  }

  return JobPriority{
      .cls = cls,
      .optimistic = optimistic,
      .frame_num = frame_num,
      .image_id = image.id().value(),
  };
}

bool ImageImporterImpl::IsJobBetter(const JobPriority& a, const JobPriority& b,
                                    uint64_t frames_composed) const {
  if (a.cls != b.cls) {
    return std::to_underlying(a.cls) < std::to_underlying(b.cls);
  }
  if (a.cls == JobClass::kNamedByPresent) {
    // `kOptimistic` outranks `kWaitForImports` in class 1 with no aging so
    // untrusted presents never add latency to Composer HAL frames
    // (DESIGN.md#priority).
    if (a.optimistic != b.optimistic) {
      return a.optimistic;
    }
    if (a.frame_num != b.frame_num) {
      // `kOptimistic` jobs prefer the larger `frame_num` (newest frame first, which matters when
      // `frames_composed_` advances during a scan) and `kWaitForImports` jobs prefer the smaller
      // one (oldest waiting present first).
      return a.optimistic ? (a.frame_num > b.frame_num) : (a.frame_num < b.frame_num);
    }
    return a.image_id < b.image_id;
  }

  if (a.cls == JobClass::kUsed) {
    const bool a_current = a.frame_num >= frames_composed;
    const bool b_current = b.frame_num >= frames_composed;
    if (a_current != b_current) {
      return a_current;
    }
    if (a.frame_num != b.frame_num) {
      if (a_current || config_.off_screen_order == OffScreenOrder::kMostRecentFirst) {
        return a.frame_num > b.frame_num;
      }
      return a.frame_num < b.frame_num;
    }
  }

  if (a.optimistic != b.optimistic) {
    return a.optimistic;
  }
  return a.image_id < b.image_id;
}

std::optional<std::pair<size_t, ImageImporterImpl::JobPriority>>
ImageImporterImpl::FindBestEligibleJobLocked(JobKind kind) const {
  const auto& queue = (kind == JobKind::kDisplay) ? display_queue_ : gpu_queue_;
  TRACE_DURATION("gfx", "ImageImporterImpl::FindBestEligibleJobLocked", "kind",
                 kind == JobKind::kDisplay ? "display" : "gpu", "queue_size", queue.size());
  const uint64_t frames = frames_composed_.load(std::memory_order_relaxed);
  const JobKind predicted = PredictedNextKind();
  const bool allow_class_3 = NeverShownEligibleLocked();

  std::optional<std::pair<size_t, JobPriority>> best;
  for (size_t i = 0; i < queue.size(); ++i) {
    const ImportedImage& img = *queue[i];
    if (!IsQueuedJob(kind, img)) {
      continue;
    }
    // Avoid calling `ComputeJobPriority()` and pulling in the cold cache line (policy, id)
    // when never-shown images (`stamp == 0`) are ineligible during active rendering.
    if (!allow_class_3 && img.frame_stamp() == 0) {
      continue;
    }

    const JobPriority prio = ComputeJobPriority(img, kind, frames, predicted);
    if (prio.cls == JobClass::kNeverNamed && !allow_class_3) {
      continue;
    }

    if (!best.has_value() || IsJobBetter(prio, best->second, frames)) {
      best = std::make_pair(i, prio);
    }
  }
  return best;
}

bool ImageImporterImpl::CanStartKindNextLocked(JobKind kind) const {
  if (stopping_.load(std::memory_order_relaxed)) {
    return false;
  }
  // Callers re-run `FindBestEligibleJobLocked()` when popping because realistic
  // queues hold 0-10 jobs and `FindBestEligibleJobLocked()`'s `TRACE_DURATION`
  // measures whether caching the index is ever worth the coupling.
  const auto candidate = FindBestEligibleJobLocked(kind);
  if (!candidate.has_value()) {
    return false;
  }
  if (config_.import_kinds == ImportKinds::kConcurrent) {
    return true;
  }

  const uint32_t disp_running = display_imports_in_flight_.load(std::memory_order_acquire);
  if (kind == JobKind::kGpu && disp_running > 0) {
    return false;
  }
  if (kind == JobKind::kDisplay && gpu_jobs_running_ > 0) {
    return false;
  }

  // A GPU job does not start while a higher-ranked display job is queued, because a
  // multi-millisecond GPU job would delay that display `ImportImage()` call (DESIGN.md#dispatcher).
  const JobKind other_kind = (kind == JobKind::kDisplay) ? JobKind::kGpu : JobKind::kDisplay;
  const auto other = FindBestEligibleJobLocked(other_kind);
  if (!other.has_value()) {
    return true;
  }
  if (candidate->second.cls != other->second.cls) {
    return std::to_underlying(candidate->second.cls) < std::to_underlying(other->second.cls);
  }
  return PredictedNextKind() == kind;
}

bool ImageImporterImpl::HasQueuedStampZeroJobLocked() const {
  const auto never_named_job = [](JobKind kind) {
    return [kind](const auto& img) { return IsQueuedJob(kind, *img) && img->frame_stamp() == 0; };
  };
  return std::ranges::any_of(display_queue_, never_named_job(JobKind::kDisplay)) ||
         std::ranges::any_of(gpu_queue_, never_named_job(JobKind::kGpu));
}

// static
bool ImageImporterImpl::IsQueuedJob(JobKind kind, const ImportedImage& image) {
  const ImportWord w = image.word();
  const ImportState state = (kind == JobKind::kDisplay) ? w.display : w.gpu;
  return !w.released && state == ImportState::kQueued;
}

void ImageImporterImpl::PruneStaleQueueEntriesLocked() {
  std::erase_if(display_queue_,
                [](const auto& img) { return !IsQueuedJob(JobKind::kDisplay, *img); });
  std::erase_if(gpu_queue_, [](const auto& img) { return !IsQueuedJob(JobKind::kGpu, *img); });
  queued_jobs_.store(static_cast<uint32_t>(display_queue_.size() + gpu_queue_.size()),
                     std::memory_order_release);
}

size_t ImageImporterImpl::QueuedJobCountLocked(JobKind kind) const {
  const auto& queue = (kind == JobKind::kDisplay) ? display_queue_ : gpu_queue_;
  return static_cast<size_t>(
      std::ranges::count_if(queue, [kind](const auto& img) { return IsQueuedJob(kind, *img); }));
}

std::shared_ptr<ImportedImage> ImageImporterImpl::PopQueueAtLocked(JobKind kind, size_t index) {
  auto& queue = (kind == JobKind::kDisplay) ? display_queue_ : gpu_queue_;
  FX_CHECK(index < queue.size());
  std::shared_ptr<ImportedImage> job = std::move(queue[index]);
  queue[index] = std::move(queue.back());
  queue.pop_back();
  EmitQueueTraceCountersLocked();
  return job;
}

std::shared_ptr<ImportedImage> ImageImporterImpl::PopBestGpuJobForWorkerLocked() {
  const auto best = FindBestEligibleJobLocked(JobKind::kGpu);
  if (!best.has_value()) {
    return nullptr;
  }
  // Count the job before `PopQueueAtLocked()` so its trace counter reflects both changes.
  ++gpu_jobs_running_;
  return PopQueueAtLocked(JobKind::kGpu, best->first);
}

void ImageImporterImpl::EmitQueueTraceCountersLocked() {
  queued_jobs_.store(static_cast<uint32_t>(display_queue_.size() + gpu_queue_.size()),
                     std::memory_order_release);
  // `TRACE_COUNTER` evaluates the counts only when `"gfx"` tracing is enabled.
  TRACE_COUNTER("gfx", "ImageImporter::Queues", 0, "display_queued",
                QueuedJobCountLocked(JobKind::kDisplay), "gpu_queued",
                QueuedJobCountLocked(JobKind::kGpu), "gpu_running", gpu_jobs_running_);
}

void ImageImporterImpl::ScheduleDispatcher() {
  if (stopping_.load(std::memory_order_relaxed)) {
    return;
  }
  // Uses `async::PostTask()` guarded by `dispatch_posted_` rather than a member
  // `async::TaskClosure` because `ScheduleDispatcher()` is called from session,
  // worker, and reply threads while `async::TaskBase` is single-threaded.
  if (!dispatch_posted_.exchange(true, std::memory_order_acq_rel)) {
    async::PostTask(main_dispatcher_, [weak_this = weak_from_this()] {
      TRACE_DURATION("gfx", "ImageImporterImpl::ScheduleDispatcher[post]");
      if (auto self = weak_this.lock()) {
        self->RunDispatcher();
      }
    });
  }
}

void ImageImporterImpl::RunDispatcher() {
  FX_DCHECK(utils::IsOnMainThread());
  const bool was_posted = dispatch_posted_.exchange(false, std::memory_order_acq_rel);
  if (stopped_ || stopping_.load(std::memory_order_relaxed)) {
    return;
  }
  FX_DCHECK(was_posted) << "RunDispatcher() runs only as the task ScheduleDispatcher() posts";

  TRACE_DURATION("gfx", "ImageImporterImpl::RunDispatcher");

  wakeup_retry_task_.Cancel();
  if (queued_jobs_.load(std::memory_order_acquire) == 0) {
    return;
  }

  const std::optional<zx::time> wakeup = next_wakeup_();

  // Make display `ImportImage()` calls up to `display_import_cap` while the
  // last frame's display config is not deferring calls and while they fit
  // before `wakeup` (DESIGN.md#dispatcher).
  for (;;) {
    std::shared_ptr<ImportedImage> display_job;
    bool deferred_until_frame_displayed = false;
    {
      std::scoped_lock lock(queue_mutex_);
      PruneStaleQueueEntriesLocked();
      const uint32_t in_flight = display_imports_in_flight_.load(std::memory_order_acquire);
      if (in_flight < config_.display_import_cap && CanStartKindNextLocked(JobKind::kDisplay)) {
        if (undisplayed_frame_deferral_.IsDeferred(async::Now(main_dispatcher_))) {
          deferred_until_frame_displayed = true;
        } else {
          const zx::time now = async::Now(main_dispatcher_);
          const zx::duration expected =
              display_import_duration_.GetPrediction() * (static_cast<int64_t>(in_flight) + 1);
          const bool fits =
              !wakeup.has_value() || (now + expected + kWakeupSafetyMargin <= *wakeup);
          if (fits) {
            if (auto best = FindBestEligibleJobLocked(JobKind::kDisplay); best.has_value()) {
              display_job = PopQueueAtLocked(JobKind::kDisplay, best->first);
            }
          } else {
            wakeup_retry_task_.PostForTime(main_dispatcher_, std::max(*wakeup, now + zx::msec(1)));
          }
        }
      }
    }

    if (deferred_until_frame_displayed) {
      undisplayed_frame_deferral_.ArmTimeout();
      break;
    }
    if (!display_job) {
      break;
    }

    auto swapped = Transition(*display_job, [](ImportWord& w) {
      if (w.released || w.display != ImportState::kQueued) {
        return false;
      }
      w.display = ImportState::kImporting;
      return true;
    });
    if (swapped.has_value()) {
      SendDisplayImport(std::move(display_job));
    }
  }

  // Wake workers if GPU jobs are now allowed to proceed; when
  // `worker_pool_size == 0`, run at most one GPU job inline per task so posted
  // `ScheduleUpdateForSession()` tasks can run between GPU jobs and move
  // `next_wakeup_()` earlier (DESIGN.md#dispatcher).
  bool wake_workers = false;
  std::shared_ptr<ImportedImage> inline_gpu_job;
  {
    std::scoped_lock lock(queue_mutex_);
    const bool gpu_can_start = RefreshGpuCanStartLocked();
    if (config_.worker_pool_size > 0) {
      wake_workers = gpu_can_start;
    } else if (gpu_can_start) {
      if (auto best = FindBestEligibleJobLocked(JobKind::kGpu); best.has_value()) {
        const ImportedImage& candidate = *gpu_queue_[best->first];
        const bool is_first =
            !candidate.collection() ||
            !candidate.collection()->first_gpu_import_claimed_.load(std::memory_order_relaxed);
        const zx::time now = async::Now(main_dispatcher_);
        const zx::duration expected = is_first ? first_gpu_import_duration_.GetPrediction()
                                               : later_gpu_import_duration_.GetPrediction();
        const bool fits = !wakeup.has_value() || (now + expected + kWakeupSafetyMargin <= *wakeup);
        if (fits) {
          inline_gpu_job = PopQueueAtLocked(JobKind::kGpu, best->first);
        } else {
          wakeup_retry_task_.PostForTime(main_dispatcher_, std::max(*wakeup, now + zx::msec(1)));
        }
      }
    }
  }

  if (wake_workers) {
    gpu_queue_nonempty_.notify_all();
  }

  if (inline_gpu_job) {
    std::unique_lock<std::mutex> gpu_lock(inline_gpu_job->gpu_run_mutex_, std::try_to_lock);
    if (gpu_lock.owns_lock()) {
      auto swapped = Transition(*inline_gpu_job, [](ImportWord& w) {
        if (w.released || w.gpu != ImportState::kQueued) {
          return false;
        }
        w.gpu = ImportState::kImporting;
        return true;
      });
      if (swapped.has_value()) {
        ExecuteGpuImport(*inline_gpu_job, gpu_lock);
      }
    }

    // Re-post if further inline GPU or display work remains eligible.
    bool more_work = false;
    {
      std::scoped_lock lock(queue_mutex_);
      more_work = CanStartKindNextLocked(JobKind::kGpu) ||
                  (!undisplayed_frame_deferral_.IsDeferred(async::Now(main_dispatcher_)) &&
                   display_imports_in_flight_.load(std::memory_order_acquire) <
                       config_.display_import_cap &&
                   CanStartKindNextLocked(JobKind::kDisplay));
    }
    if (more_work) {
      ScheduleDispatcher();
    }
  }
}

void ImageImporterImpl::ReapGpuImage(std::unique_ptr<ImportedImage::GpuImage> gpu_image) {
  FX_DCHECK(utils::IsOnMainThread());
  if (gpu_workers_.empty() || stopping_.load(std::memory_order_relaxed)) {
    gpu_image.reset();
    return;
  }
  {
    std::scoped_lock lock(queue_mutex_);
    reaper_queue_.push_back(std::move(gpu_image));
  }
  gpu_queue_nonempty_.notify_one();
}

void ImageImporterImpl::GpuWorkerLoop(uint32_t worker_index) {
  const std::string thread_name = "scenic.image-import-" + std::to_string(worker_index);
  zx::thread::self()->set_property(ZX_PROP_NAME, thread_name.data(), thread_name.size());
  const zx_status_t role_status =
      fuchsia_scheduler::SetRoleForThisThread("fuchsia.scenic.image-import");
  if (role_status != ZX_OK) {
    FX_LOGS(WARNING) << "Failed to apply fuchsia.scenic.image-import role on " << thread_name
                     << ": " << zx_status_get_string(role_status);
  }

  for (;;) {
    std::shared_ptr<ImportedImage> job;
    std::unique_ptr<ImportedImage::GpuImage> to_reap;
    uint64_t jobs_this_wakeup = 0;

    {
      std::unique_lock<std::mutex> lock(queue_mutex_);
      gpu_queue_nonempty_.wait(lock, [this]() FXL_NO_THREAD_SAFETY_ANALYSIS {
        return stopping_.load(std::memory_order_relaxed) || !reaper_queue_.empty() ||
               (!gpu_jobs_deferred_ && CanStartKindNextLocked(JobKind::kGpu));
      });
      if (stopping_.load(std::memory_order_relaxed)) {
        return;
      }
      // `std::unique_lock` lacks libc++ `SCOPED_CAPABILITY` annotations, though
      // `wait()` guarantees `queue_mutex_` is held upon return.
      [&]() FXL_NO_THREAD_SAFETY_ANALYSIS {
        // GPU imports take priority; reap only when no GPU job is eligible.
        if (!gpu_jobs_deferred_ && CanStartKindNextLocked(JobKind::kGpu)) {
          job = PopBestGpuJobForWorkerLocked();
        }
        if (!job && !reaper_queue_.empty()) {
          to_reap = std::move(reaper_queue_.back());
          reaper_queue_.pop_back();
        }
      }();
    }

    if (to_reap) {
      TRACE_DURATION("gfx", "ImageImporterImpl::ReapGpuImage[worker]");
      to_reap.reset();
      continue;
    }
    if (!job) {
      continue;
    }
    inspect_worker_wakeups_.Add(1);

    while (job) {
      {
        std::unique_lock<std::mutex> gpu_lock(job->gpu_run_mutex_, std::try_to_lock);
        if (gpu_lock.owns_lock()) {
          auto swapped = Transition(*job, [](ImportWord& w) {
            if (w.released || w.gpu != ImportState::kQueued) {
              return false;
            }
            w.gpu = ImportState::kImporting;
            return true;
          });
          if (swapped.has_value()) {
            ExecuteGpuImport(*job, gpu_lock);
            ++jobs_this_wakeup;
            inspect_worker_jobs_completed_.Add(1);
          }
        }
      }

      std::shared_ptr<ImportedImage> next_job;
      bool schedule_main_for_display = false;
      {
        std::scoped_lock lock(queue_mutex_);
        FX_CHECK(gpu_jobs_running_ > 0);
        --gpu_jobs_running_;
        EmitQueueTraceCountersLocked();
        if (!stopping_.load(std::memory_order_relaxed) && !gpu_jobs_deferred_ &&
            CanStartKindNextLocked(JobKind::kGpu)) {
          next_job = PopBestGpuJobForWorkerLocked();
        } else if (gpu_jobs_running_ == 0 && !stopping_.load(std::memory_order_relaxed) &&
                   CanStartKindNextLocked(JobKind::kDisplay)) {
          gpu_jobs_deferred_ = true;
          schedule_main_for_display = true;
        }
      }

      if (schedule_main_for_display) {
        ScheduleDispatcher();
      }
      job = std::move(next_job);
    }

    inspect_worker_jobs_per_wakeup_.Insert(jobs_this_wakeup);
  }
}

void ImageImporterImpl::ExecuteGpuImport(ImportedImage& image,
                                         std::unique_lock<std::mutex>& gpu_lock) {
  FX_DCHECK(gpu_lock.owns_lock());
  TRACE_DURATION("gfx", "ImageImporterImpl::ExecuteGpuImport", "image_id", image.id().value(),
                 "collection_id", image.metadata().collection_id);

  const zx::time start = zx::clock::get_monotonic();
  const bool is_first =
      image.collection() &&
      !image.collection()->first_gpu_import_claimed_.exchange(true, std::memory_order_relaxed);

  fit::result<ImportImageError, std::unique_ptr<ImportedImage::GpuImage>> res =
      fit::error(ImportImageError::kCollectionFailed);
  if (image.collection() && image.collection()->gpu_collection()) {
    res = renderer_->CreateGpuImage(image.metadata(), *image.collection()->gpu_collection(),
                                    image.collection()->per_buffer_bytes());
  }
  const zx::time finished = zx::clock::get_monotonic();

  auto on_failure = std::move(image.on_failure_);
  auto reply_disp = image.reply_dispatcher_.lock();
  image.reply_dispatcher_.reset();

  if (res.is_ok()) {
    image.gpu_image_ = std::move(res.value());
    image.gpu_ready_latency_ = finished - image.created();
  } else {
    image.gpu_error_ = res.error_value();
    inspect_gpu_imports_failed_.Add(1);
  }

  const bool succeeded = res.is_ok();
  auto swapped = Transition(image, [succeeded](ImportWord& w) {
    FX_CHECK(w.gpu == ImportState::kImporting);
    w.gpu = succeeded ? ImportState::kReady : ImportState::kFailed;
    return true;
  });
  FX_CHECK(swapped.has_value());

  if (succeeded) {
    // `SafeCollectionBufferBytes()` is inlined inside `TRACE_INSTANT` rather than stored in a
    // local variable so that it is only evaluated when `"gfx"` tracing is enabled.
    TRACE_INSTANT("gfx", "ImportedImage::GpuReady", TRACE_SCOPE_THREAD, "image_id",
                  image.id().value(), "collection_id", image.metadata().collection_id, "bytes",
                  SafeCollectionBufferBytes(image.collection().get()));
    TRACE_FLOW_STEP("gfx", "flatland::ImportedImage", ImageFlowId(image.id()));
  }

  const allocation::GlobalImageId id = image.id();
  // Release `gpu_run_mutex_` right after the terminal CAS before taking importer
  // locks or posting callbacks so any waiting frame thread wakes at once.
  gpu_lock.unlock();

  if (succeeded) {
    std::scoped_lock qlock(queue_mutex_);
    const zx::duration elapsed = std::max(zx::duration{0}, finished - start);
    if (is_first) {
      first_gpu_import_duration_.InsertNewMeasurement(elapsed);
    } else {
      later_gpu_import_duration_.InsertNewMeasurement(elapsed);
    }
  }

  if (!succeeded && on_failure && reply_disp) {
    const ImportImageError code = res.error_value();
    async::PostTask(reply_disp->dispatcher(),
                    [cb = std::move(on_failure), id, code]() mutable { cb(id, code); });
  }

  ResolvePendingEntries(image);

  if (swapped->second.released && swapped->second.display != ImportState::kImporting) {
    PostCleanUp(id);
  }
}

void ImageImporterImpl::BeginFrame() {
  FX_DCHECK(utils::IsOnMainThread());
  current_frame_display_sent_wait_ = zx::duration{0};
  current_frame_display_inflight_wait_ = zx::duration{0};
  current_frame_gpu_wait_ = zx::duration{0};
}

void ImageImporterImpl::OnFrameCommitted(const FrameReport& report) {
  FX_DCHECK(utils::IsOnMainThread());
  DCheckUniqueUnreleasedImages(report.images);
  TRACE_DURATION("gfx", "ImageImporterImpl::OnFrameCommitted", "outcome",
                 static_cast<uint32_t>(report.outcome));

  frame_composed_since_last_cpu_done_ = true;
  last_frame_time_ = async::Now(main_dispatcher_);

  const uint64_t frame_num = frames_composed_.fetch_add(1, std::memory_order_relaxed) + 1;

  struct InspectDeltas {
    bool correct = false;
    bool wrong = false;
    bool from_present_hint = false;
    bool from_present_hint_correct = false;
    FrameOutcome outcome;
    uint64_t total_blocked_us = 0;
    uint64_t display_sent_us = 0;
    uint64_t display_inflight_us = 0;
    uint64_t gpu_us = 0;
  } d;
  d.outcome = report.outcome;

  const JobKind predicted = PredictedNextKind();
  const bool used_present_hint = present_named_gpu_only_.exchange(false, std::memory_order_relaxed);
  if (!report.images.empty() && report.outcome != FrameOutcome::kGpuForced) {
    const JobKind actual =
        (report.outcome == FrameOutcome::kDirect) ? JobKind::kDisplay : JobKind::kGpu;
    if (predicted == actual) {
      d.correct = true;
      if (used_present_hint) {
        d.from_present_hint_correct = true;
      }
    } else {
      d.wrong = true;
    }
    if (used_present_hint) {
      d.from_present_hint = true;
    }
  }

  if (report.outcome == FrameOutcome::kGpuForced || !report.images.empty()) {
    last_outcome_.store(report.outcome, std::memory_order_relaxed);
  }

  for (ImportedImage* img : report.images) {
    const ImportWord w = img->word();
    if (!IsTerminalImportState(w.display) || !IsTerminalImportState(w.gpu)) {
      img->frame_stamp_.store(frame_num, std::memory_order_relaxed);
    }

    const bool had_any_use = (img->direct_frames_ > 0 || img->gpu_frames_ > 0);
    if (report.outcome == FrameOutcome::kDirect) {
      if (img->direct_frames_++ == 0) {
        if (!had_any_use) {
          TRACE_FLOW_STEP("gfx", "flatland::ImportedImage", ImageFlowId(img->id()));
        }
        TRACE_INSTANT("gfx", "ImportedImage::FirstDirectUse", TRACE_SCOPE_THREAD, "image_id",
                      img->id().value(), "collection_id", img->metadata().collection_id, "bytes",
                      SafeCollectionBufferBytes(img->collection().get()));
      }
    } else {
      if (img->gpu_frames_++ == 0) {
        if (!had_any_use) {
          TRACE_FLOW_STEP("gfx", "flatland::ImportedImage", ImageFlowId(img->id()));
        }
        TRACE_INSTANT("gfx", "ImportedImage::FirstGpuUse", TRACE_SCOPE_THREAD, "image_id",
                      img->id().value(), "collection_id", img->metadata().collection_id, "bytes",
                      SafeCollectionBufferBytes(img->collection().get()));
      }
    }
  }

  idle_.store(false, std::memory_order_relaxed);

  if (report.config_stamp.value != fuchsia_hardware_display::wire::kInvalidConfigStampValue &&
      config_.enable_direct_to_display) {
    undisplayed_frame_deferral_.OnFrameCommitted(
        report.config_stamp, report.target_presentation_time + report.vsync_interval);
  } else {
    undisplayed_frame_deferral_.Stop();
  }

  const zx::duration total_blocked = current_frame_display_sent_wait_ +
                                     current_frame_display_inflight_wait_ + current_frame_gpu_wait_;
  d.total_blocked_us = ToMicrosClamped(total_blocked);
  d.display_sent_us = ToMicrosClamped(current_frame_display_sent_wait_);
  d.display_inflight_us = ToMicrosClamped(current_frame_display_inflight_wait_);
  d.gpu_us = ToMicrosClamped(current_frame_gpu_wait_);

  inspect_node_.AtomicUpdate([this, &d](inspect::Node&) {
    if (d.correct)
      inspect_predictions_correct_.Add(1);
    if (d.wrong)
      inspect_predictions_wrong_.Add(1);
    if (d.from_present_hint)
      inspect_predictions_from_present_hint_.Add(1);
    if (d.from_present_hint_correct)
      inspect_predictions_from_present_hint_correct_.Add(1);

    switch (d.outcome) {
      case FrameOutcome::kDirect:
        inspect_frames_direct_.Add(1);
        break;
      case FrameOutcome::kGpuForced:
        inspect_frames_gpu_forced_.Add(1);
        break;
      case FrameOutcome::kTooManyLayers:
        inspect_frames_fallback_too_many_layers_.Add(1);
        break;
      case FrameOutcome::kConfigRejected:
        inspect_frames_fallback_config_rejected_.Add(1);
        break;
      case FrameOutcome::kGpuOnlyImage:
        inspect_frames_fallback_gpu_only_image_.Add(1);
        break;
      case FrameOutcome::kDisplayImportFailed:
        inspect_frames_fallback_display_import_failed_.Add(1);
        break;
    }

    inspect_frame_blocked_total_us_.Insert(d.total_blocked_us);
    inspect_frame_blocked_display_sent_us_.Insert(d.display_sent_us);
    inspect_frame_blocked_display_in_flight_us_.Insert(d.display_inflight_us);
    inspect_frame_blocked_gpu_us_.Insert(d.gpu_us);
  });

  TRACE_COUNTER("gfx", "ImageImporter::FrameBlockedUs", 0, "display_sent_us", d.display_sent_us,
                "display_in_flight_us", d.display_inflight_us, "gpu_us", d.gpu_us);
  frame_blocked_counter_nonzero_ = total_blocked > zx::duration{0};
  current_frame_display_sent_wait_ = zx::duration{0};
  current_frame_display_inflight_wait_ = zx::duration{0};
  current_frame_gpu_wait_ = zx::duration{0};
}

void ImageImporterImpl::OnCpuWorkDone() {
  FX_DCHECK(utils::IsOnMainThread());
  if (!frame_composed_since_last_cpu_done_) {
    inspect_frames_skipped_.Add(1);
    undisplayed_frame_deferral_.OnFrameSkipped();
  }
  frame_composed_since_last_cpu_done_ = false;
  ScheduleDispatcher();
}

void ImageImporterImpl::OnVsync(display::WireConfigStamp stamp, zx::time_monotonic timestamp) {
  FX_DCHECK(utils::IsOnMainThread());
  // Zero the counter at the first vsync after a blocked frame, so in a trace the nonzero sample
  // spans only that frame's render and target vsync instead of lasting until the next frame.
  if (frame_blocked_counter_nonzero_) {
    frame_blocked_counter_nonzero_ = false;
    TRACE_COUNTER("gfx", "ImageImporter::FrameBlockedUs", 0, "display_sent_us", uint64_t{0},
                  "display_in_flight_us", uint64_t{0}, "gpu_us", uint64_t{0});
  }

  const bool stamp_released = undisplayed_frame_deferral_.OnStampDisplayed(stamp);

  bool schedule_main = false;
  bool wake_gpu = false;

  bool idle_became_true = false;
  if (config_.never_shown_imports == NeverShownImports::kWhenIdle &&
      !idle_.load(std::memory_order_relaxed) &&
      timestamp - last_frame_time_ >= config_.idle_after) {
    idle_.store(true, std::memory_order_relaxed);
    idle_became_true = true;
  }

  if ((stamp_released || idle_became_true) && queued_jobs_.load(std::memory_order_acquire) > 0) {
    std::scoped_lock lock(queue_mutex_);
    if (stamp_released && CanStartKindNextLocked(JobKind::kDisplay)) {
      schedule_main = true;
    }
    if (idle_became_true) {
      if (HasQueuedStampZeroJobLocked()) {
        // `idle_` makes class-3 jobs eligible, which may allow deferred GPU jobs to start.
        wake_gpu = RefreshGpuCanStartLocked();
        schedule_main = true;
      }
    }
  }

  if (wake_gpu) {
    WakeGpuRunner();
  }
  if (schedule_main) {
    ScheduleDispatcher();
  }
}

void ImageImporterImpl::PostCleanUp(allocation::GlobalImageId id) {
  async::PostTask(main_dispatcher_, [weak_this = weak_from_this(), id] {
    if (auto self = weak_this.lock()) {
      self->CleanUpById(id);
    }
  });
}

void ImageImporterImpl::CleanUpById(allocation::GlobalImageId id) {
  FX_DCHECK(utils::IsOnMainThread());
  std::shared_ptr<ImportedImage> img;
  {
    std::scoped_lock lock(collections_mutex_);
    if (auto it = live_images_by_id_.find(id); it != live_images_by_id_.end()) {
      img = it->second;
    }
  }
  if (img) {
    CleanUp(img);
  }
}

void ImageImporterImpl::CleanUp(const std::shared_ptr<ImportedImage>& image) {
  FX_CHECK(utils::IsOnMainThread());
  TRACE_DURATION("gfx", "ImageImporterImpl::CleanUp", "image_id", image->id().value());
  auto swapped = Transition(*image, [](ImportWord& w) {
    if (w.display == ImportState::kReleased && w.gpu == ImportState::kReleased) {
      return false;
    }
    FX_CHECK(w.display != ImportState::kImporting && w.gpu != ImportState::kImporting);
    w.display = ImportState::kReleased;
    w.gpu = ImportState::kReleased;
    w.released = true;
    return true;
  });
  if (!swapped.has_value()) {
    return;
  }

  const ImportWord old_word = swapped->first;
  const bool was_sampled =
      (image->gpu_image_ && image->gpu_image_->WasSampled()) || image->gpu_frames_ > 0;

  if (old_word.display == ImportState::kReady && image->direct_frames_ == 0) {
    inspect_wasted_display_imports_.Add(1);
  }
  if (old_word.gpu == ImportState::kReady && !was_sampled) {
    inspect_wasted_gpu_imports_.Add(1);
  }
  TRACE_FLOW_END("gfx", "flatland::ImportedImage", ImageFlowId(image->id()));

  const int64_t disp_ready_us =
      image->display_ready_latency_.to_nsecs() >= 0 ? image->display_ready_latency_.to_usecs() : -1;
  const int64_t gpu_ready_us =
      image->gpu_ready_latency_.to_nsecs() >= 0 ? image->gpu_ready_latency_.to_usecs() : -1;
  // `SafeCollectionBufferBytes()` is inlined inside `TRACE_INSTANT` rather than stored in a
  // local variable so that it is only evaluated when `"gfx"` tracing is enabled.
  TRACE_INSTANT("gfx", "ImportedImage::CleanUpSummary", TRACE_SCOPE_THREAD, "image_id",
                image->id().value(), "bytes", SafeCollectionBufferBytes(image->collection().get()),
                "display_ready_us", disp_ready_us, "gpu_ready_us", gpu_ready_us, "direct_frames",
                image->direct_frames_, "gpu_frames", image->gpu_frames_);

  if (old_word.display == ImportState::kReady && coordinator_) {
    coordinator_->ReleaseImage(image->id());
  }

  {
    TRACE_DURATION("gfx", "ImageImporterImpl::ResetGpuImage", "image_id", image->id().value());
    std::unique_ptr<ImportedImage::GpuImage> gpu_image = std::move(image->gpu_image_);
    // Destroying an unsampled image costs a synchronous GPU round trip in some drivers, so
    // destroy it off main. A sampled image is destroyed here, which hands it to the
    // renderer's main-thread recycler (DESIGN.md#renderer-contract).
    if (gpu_image && !gpu_image->WasSampled()) {
      ReapGpuImage(std::move(gpu_image));
    }
  }
  image->on_failure_ = nullptr;
  image->reply_dispatcher_.reset();

  // Drain any remaining `PendingEntry` waiters so a released image never blocks
  // a waiting present (DESIGN.md#cleanup-on-main).
  bool hint_set = false;
  {
    const zx::time now = zx::clock::get_monotonic();
    std::scoped_lock lock(pending_signal_mutex_);
    hint_set = SignalPendingEntriesLocked(*image, now);
  }
  if (hint_set) {
    bool wake_gpu = false;
    {
      std::scoped_lock lock(queue_mutex_);
      const bool was_deferred = gpu_jobs_deferred_;
      wake_gpu = RefreshGpuCanStartLocked() && was_deferred;
    }
    if (wake_gpu) {
      WakeGpuRunner();
    } else {
      ScheduleDispatcher();
    }
  }

  std::shared_ptr<ImportedCollection> coll_to_tear_down;
  {
    std::scoped_lock lock(collections_mutex_);
    live_images_by_id_.erase(image->id());
    if (const auto& coll = image->collection()) {
      std::erase(coll->waiting_for_allocation_, image);
      FX_CHECK(coll->live_images_ > 0);
      if (--coll->live_images_ == 0 && coll->released_by_allocator_) {
        if (auto it = collections_.find(coll->id()); it != collections_.end()) {
          coll_to_tear_down = std::move(it->second);
          collections_.erase(it);
        }
      }
    }
  }

  if (coll_to_tear_down) {
    TearDown(*coll_to_tear_down);
  }

  if (!stopping_.load(std::memory_order_relaxed)) {
    bool wake_gpu = false;
    {
      std::scoped_lock lock(queue_mutex_);
      PruneStaleQueueEntriesLocked();
      const bool was_deferred = gpu_jobs_deferred_;
      wake_gpu = RefreshGpuCanStartLocked() && was_deferred;
    }
    if (wake_gpu) {
      WakeGpuRunner();
    }
  }
}

void ImageImporterImpl::ReleaseBufferCollection(allocation::GlobalBufferCollectionId collection_id,
                                                allocation::BufferCollectionUsage usage) {
  FX_CHECK(utils::IsOnMainThread());
  FX_CHECK(usage == allocation::BufferCollectionUsage::kClientImage);
  if (stopping_.load(std::memory_order_relaxed)) {
    return;
  }

  if (auto pending_it = pending_registrations_.find(collection_id);
      pending_it != pending_registrations_.end()) {
    pending_it->second->released_by_allocator = true;
    return;
  }

  std::shared_ptr<ImportedCollection> coll_to_tear_down;
  {
    std::scoped_lock lock(collections_mutex_);
    auto it = collections_.find(collection_id);
    if (it == collections_.end()) {
      return;
    }
    it->second->released_by_allocator_ = true;
    if (it->second->live_images_ == 0) {
      coll_to_tear_down = std::move(it->second);
      collections_.erase(it);
    }
  }

  if (coll_to_tear_down) {
    TearDown(*coll_to_tear_down);
  }
}

void ImageImporterImpl::TearDown(ImportedCollection& collection) {
  FX_CHECK(utils::IsOnMainThread());
  TRACE_DURATION("gfx", "ImageImporterImpl::TearDown", "collection_id", collection.id());

  if (collection.state() == ImportedCollection::State::kReleased) {
    return;
  }
  FX_CHECK(collection.live_images_ == 0);
  FX_CHECK(collection.waiting_for_allocation_.empty());

  ReleaseSysmemClient(collection.allocation_participant_);
  ReleaseSysmemClient(collection.display_participant_);
  collection.gpu_collection_.reset();

  if (config_.enable_direct_to_display && coordinator_) {
    coordinator_->ReleaseBufferCollection(
        display::ToDisplayFidlBufferCollectionId(collection.id()));
  }
  collection.state_.store(ImportedCollection::State::kReleased, std::memory_order_release);
}

fpromise::promise<> ImageImporterImpl::ImportBufferImage(
    const allocation::ImageMetadata& /*metadata*/, allocation::BufferCollectionUsage /*usage*/) {
  FX_CHECK(false) << "Client images must be imported through ImageImporter::ImportImage()";
  return fpromise::make_error_promise();
}

void ImageImporterImpl::ReleaseBufferImage(allocation::GlobalImageId /*image_id*/) {
  FX_CHECK(false) << "Client images must be released through ImageImporter::ReleaseImage()";
}

}  // namespace flatland
