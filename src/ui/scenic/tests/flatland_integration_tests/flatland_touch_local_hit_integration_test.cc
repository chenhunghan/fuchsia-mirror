// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.math/cpp/fidl.h>
#include <fidl/fuchsia.ui.composition/cpp/fidl.h>
#include <fidl/fuchsia.ui.pointer.augment/cpp/fidl.h>
#include <fidl/fuchsia.ui.pointer/cpp/fidl.h>
#include <fidl/fuchsia.ui.pointerinjector/cpp/fidl.h>
#include <fidl/fuchsia.ui.views/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/ui/scenic/cpp/view_creation_tokens.h>
#include <lib/ui/scenic/cpp/view_identity.h>
#include <lib/ui/scenic/cpp/view_ref_pair.h>
#include <zircon/status.h>

#include <algorithm>
#include <functional>
#include <iterator>
#include <memory>
#include <utility>
#include <vector>

#include <zxtest/zxtest.h>

#include "src/ui/scenic/tests/utils/blocking_present.h"
#include "src/ui/scenic/tests/utils/flatland_client_with_event_handler.h"
#include "src/ui/scenic/tests/utils/scenic_ctf_test_base.h"
#include "src/ui/scenic/tests/utils/simple_watcher_client.h"
#include "src/ui/scenic/tests/utils/utils.h"

// These tests exercise the integration between Flatland and the InputSystem for
// TouchSourceWithLocalHit. Setup:
// - Injection done in context View Space, with fuchsia.ui.pointerinjector
// - Target(s) specified by View (using view ref koids)
// - Dispatch done to fuchsia.ui.pointer.TouchSourceWithLocalHit in receiver(s') View Space.

// TODO(https://fxbug.dev/42071876): Investigate GFX view tree getting tickled.

namespace integration_tests {

namespace fuc = fuchsia_ui_composition;
namespace fup = fuchsia_ui_pointer;
namespace fupa = fuchsia_ui_pointer_augment;
namespace fupi = fuchsia_ui_pointerinjector;
namespace fuv = fuchsia_ui_views;

namespace {
const fuc::TransformId kRootTransform(1);
const fuc::ContentId kRootContentId(1);
}  // namespace

class FlatlandTouchLocalHitIntegrationTest : public ScenicCtfTest {
 protected:
  static constexpr uint32_t kDeviceId = 1111;
  static constexpr uint32_t kPointerId = 2222;

  // clang-format off
  static constexpr std::array<float, 9> kIdentityMatrix = {
    1, 0, 0, // column one
    0, 1, 0, // column two
    0, 0, 1, // column three
  };
  // clang-format on

  void SetUp() override {
    ScenicCtfTest::SetUp();

    pointerinjector_registry_ = ConnectSyncIntoRealm<fupi::Registry>();
    local_hit_registry_ =
        fidl::Client<fupa::LocalHit>(ConnectIntoRealm<fupa::LocalHit>(), dispatcher());

    // Set up root view and root transform.
    root_instance_ = std::make_unique<FlatlandClientWithEventHandler>(
        ConnectIntoRealm<fuc::Flatland>(), dispatcher());
    root_instance_->set_on_close(FailOnClose("Lost connection to Scenic"));

    auto [child_token, parent_token] = scenic::cpp::ViewCreationTokenPair::New();
    auto [parent_viewport_watcher_client_end, parent_viewport_watcher_server_end] =
        fidl::CreateEndpoints<fuc::ParentViewportWatcher>().value();
    auto identity = scenic::cpp::NewViewIdentityOnCreation();
    root_view_ref_ = scenic::cpp::CloneViewRef(identity.view_ref());

    ASSERT_TRUE((*root_instance_)
                    ->CreateView2({{.token = std::move(child_token),
                                    .view_identity = std::move(identity),
                                    .protocols = {},
                                    .parent_viewport_watcher =
                                        std::move(parent_viewport_watcher_server_end)}})
                    .is_ok());

    SetFlatlandDisplayContent(std::move(parent_token));

    ASSERT_TRUE((*root_instance_)->CreateTransform({{.transform_id = kRootTransform}}).is_ok());
    ASSERT_TRUE((*root_instance_)->SetRootTransform({{.transform_id = kRootTransform}}).is_ok());
    BlockingPresent(this, *root_instance_);

    // Get the display's width and height. Since there is no Present in FlatlandDisplay, receiving
    // this callback ensures that all FlatlandDisplay calls are processed.
    fidl::Client<fuc::ParentViewportWatcher> parent_viewport_watcher(
        std::move(parent_viewport_watcher_client_end), dispatcher());
    std::optional<fuc::LayoutInfo> info;
    parent_viewport_watcher->GetLayout().Then(
        [&info](fidl::Result<fuc::ParentViewportWatcher::GetLayout>& result) {
          if (result.is_ok()) {
            info = std::move(result->info());
          }
        });
    RunLoopUntil([&info] { return info.has_value(); });
    display_width_ = static_cast<float>(info->logical_size()->width());
    display_height_ = static_cast<float>(info->logical_size()->height());
  }

  void RegisterInjector(fuv::ViewRef context_view_ref, fuv::ViewRef target_view_ref) {
    fupi::Config config;
    config.device_id(kDeviceId);
    config.device_type(fupi::DeviceType::kTouch);
    config.dispatch_policy(fupi::DispatchPolicy::kTopHitAndAncestorsInTarget);

    config.context(fupi::Context::WithView(std::move(context_view_ref)));
    config.target(fupi::Target::WithView(std::move(target_view_ref)));

    fupi::Viewport viewport;
    viewport.extents(FullScreenExtents());
    viewport.viewport_to_context_transform(kIdentityMatrix);
    config.viewport(std::move(viewport));

    auto [injector_client_end, injector_server_end] = fidl::CreateEndpoints<fupi::Device>().value();
    injector_ = std::make_unique<SimpleWatcherClient<fupi::Device>>(std::move(injector_client_end),
                                                                    dispatcher());

    auto result = pointerinjector_registry_->Register(
        {{.config = std::move(config), .injector = std::move(injector_server_end)}});
    ASSERT_TRUE(result.is_ok());
    ASSERT_TRUE(injector_->is_bound());
  }

  void Inject(float x, float y, fupi::EventPhase phase) {
    FX_CHECK(injector_);
    fupi::Event event;
    event.timestamp(0);
    {
      fupi::PointerSample pointer_sample;
      pointer_sample.pointer_id(kPointerId);
      pointer_sample.phase(phase);
      pointer_sample.position_in_viewport(std::array<float, 2>{x, y});
      event.data(fupi::Data::WithPointerSample(std::move(pointer_sample)));
    }
    std::vector<fupi::Event> events;
    events.emplace_back(std::move(event));
    bool hanging_get_returned = false;
    (*injector_)
        ->Inject({{.events = std::move(events)}})
        .Then([&hanging_get_returned](fidl::Result<fupi::Device::Inject>& result) {
          hanging_get_returned = true;
        });
    RunLoopUntil(
        [this, &hanging_get_returned] { return hanging_get_returned || !injector_->is_bound(); });
  }

  // Starts a recursive TouchSource::Watch() loop that collects all received events into
  // |out_events|.
  void StartWatchLoop(SimpleWatcherClient<fupa::TouchSourceWithLocalHit>& touch_source,
                      std::vector<fupa::TouchEventWithLocalHit>& out_events,
                      fup::TouchResponseType response_type = fup::TouchResponseType::kMaybe) {
    const size_t index = watch_loops_.size();
    watch_loops_.emplace_back();
    watch_loops_.at(index) = [this, &touch_source, &out_events, response_type,
                              index](std::vector<fupa::TouchEventWithLocalHit> events) {
      std::vector<fup::TouchResponse> responses;
      for (auto& event : events) {
        if (event.touch_event().pointer_sample().has_value()) {
          fup::TouchResponse response;
          response.response_type(response_type);
          responses.emplace_back(std::move(response));
        } else {
          responses.emplace_back(fup::TouchResponse{});
        }
      }
      std::move(events.begin(), events.end(), std::back_inserter(out_events));

      touch_source->Watch({{.responses = std::move(responses)}})
          .Then([this, index](fidl::Result<fupa::TouchSourceWithLocalHit::Watch>& result) {
            if (result.is_ok()) {
              watch_loops_.at(index)(std::move(result->events()));
            }
          });
    };
    touch_source->Watch({{.responses = {}}})
        .Then([this, index](fidl::Result<fupa::TouchSourceWithLocalHit::Watch>& result) {
          if (result.is_ok()) {
            watch_loops_.at(index)(std::move(result->events()));
          }
        });
  }

  // Starts a recursive `TouchSource::Watch()` loop that collects all received events into
  // `out_events`.
  void StartWatchLoop(SimpleWatcherClient<fup::TouchSource>& touch_source,
                      std::vector<fup::TouchEvent>& out_events,
                      fup::TouchResponseType response_type = fup::TouchResponseType::kMaybe) {
    const size_t index = touch_watch_loops_.size();
    touch_watch_loops_.emplace_back();
    touch_watch_loops_.at(index) = [this, &touch_source, &out_events, response_type,
                                    index](std::vector<fup::TouchEvent> events) {
      std::vector<fup::TouchResponse> responses;
      for (auto& event : events) {
        if (event.pointer_sample().has_value()) {
          fup::TouchResponse response;
          response.response_type(response_type);
          responses.emplace_back(std::move(response));
        } else {
          responses.emplace_back(fup::TouchResponse{});
        }
      }
      std::move(events.begin(), events.end(), std::back_inserter(out_events));

      touch_source->Watch({{.responses = std::move(responses)}})
          .Then([this, index](fidl::Result<fup::TouchSource::Watch>& result) {
            if (result.is_ok()) {
              touch_watch_loops_.at(index)(std::move(result->events()));
            }
          });
    };
    touch_source->Watch({{.responses = {}}})
        .Then([this, index](fidl::Result<fup::TouchSource::Watch>& result) {
          if (result.is_ok()) {
            touch_watch_loops_.at(index)(std::move(result->events()));
          }
        });
  }

  // Returns `nullptr` (after recording a test failure) if the upgrade fails, so callers can
  // `ASSERT_NE()` instead of waiting on a client that will never be created.
  std::unique_ptr<SimpleWatcherClient<fupa::TouchSourceWithLocalHit>> UpgradeTouchSource(
      fidl::ClientEnd<fup::TouchSource> original) {
    std::unique_ptr<SimpleWatcherClient<fupa::TouchSourceWithLocalHit>> upgraded;
    bool upgrade_done = false;
    local_hit_registry_->Upgrade({{.original = std::move(original)}})
        .Then([this, &upgraded, &upgrade_done](fidl::Result<fupa::LocalHit::Upgrade>& result) {
          upgrade_done = true;
          ASSERT_TRUE(result.is_ok());
          ASSERT_FALSE(result->error().has_value());
          ASSERT_TRUE(result->augmented().is_valid());
          upgraded = std::make_unique<SimpleWatcherClient<fupa::TouchSourceWithLocalHit>>(
              std::move(result->augmented()), dispatcher(), FailOnClose("Touch source closed"));
        });
    RunLoopUntil([&upgraded, &upgrade_done] {
      return upgrade_done && (upgraded == nullptr || upgraded->is_bound());
    });
    return upgraded;
  }

  // Convenience function, we assume the test constructs topologies with one level of N children.
  // Prereq: |parent_of_viewport_transform| is created and connected to the view's root.
  fuv::ViewRef CreateAndAddChildView(
      FlatlandClientWithEventHandler& parent_instance, fuc::TransformId viewport_transform_id,
      fuchsia_math::Rect viewport_spec, fuc::TransformId parent_of_viewport_transform,
      fuc::ContentId viewport_content_id,
      std::unique_ptr<FlatlandClientWithEventHandler>& child_instance,
      fidl::ServerEnd<fup::TouchSource> child_touch_source = {}) {
    child_instance = std::make_unique<FlatlandClientWithEventHandler>(
        ConnectIntoRealm<fuc::Flatland>(), dispatcher());
    child_instance->set_on_close(FailOnClose("Lost connection to Scenic"));

    // Set up the child view watcher.
    auto [child_view_watcher_client_end, child_view_watcher_server_end] =
        fidl::CreateEndpoints<fuc::ChildViewWatcher>().value();
    auto [child_token, parent_token] = scenic::cpp::ViewCreationTokenPair::New();
    fuc::ViewportProperties properties;
    properties.logical_size(fuchsia_math::SizeU(static_cast<uint32_t>(viewport_spec.width()),
                                                static_cast<uint32_t>(viewport_spec.height())));

    EXPECT_TRUE(
        parent_instance->CreateTransform({{.transform_id = viewport_transform_id}}).is_ok());
    EXPECT_TRUE(
        parent_instance
            ->CreateViewport({{.viewport_id = viewport_content_id,
                               .token = std::move(parent_token),
                               .properties = std::move(properties),
                               .child_view_watcher = std::move(child_view_watcher_server_end)}})
            .is_ok());
    EXPECT_TRUE(parent_instance
                    ->SetContent({{.transform_id = viewport_transform_id,
                                   .content_id = viewport_content_id}})
                    .is_ok());
    EXPECT_TRUE(parent_instance
                    ->AddChild({{.parent_transform_id = parent_of_viewport_transform,
                                 .child_transform_id = viewport_transform_id}})
                    .is_ok());
    EXPECT_TRUE(parent_instance
                    ->SetTranslation(
                        {{.transform_id = viewport_transform_id,
                          .translation = fuchsia_math::Vec(viewport_spec.x(), viewport_spec.y())}})
                    .is_ok());

    BlockingPresent(this, parent_instance);

    // Set up the child view along with its TouchSource channel.
    auto [parent_viewport_watcher_client_end, parent_viewport_watcher_server_end] =
        fidl::CreateEndpoints<fuc::ParentViewportWatcher>().value();
    auto identity = scenic::cpp::NewViewIdentityOnCreation();
    auto child_view_ref = scenic::cpp::CloneViewRef(identity.view_ref());
    fuc::ViewBoundProtocols protocols;
    if (child_touch_source.is_valid()) {
      protocols.touch_source(std::move(child_touch_source));
    }
    EXPECT_TRUE((*child_instance)
                    ->CreateView2({{.token = std::move(child_token),
                                    .view_identity = std::move(identity),
                                    .protocols = std::move(protocols),
                                    .parent_viewport_watcher =
                                        std::move(parent_viewport_watcher_server_end)}})
                    .is_ok());
    EXPECT_TRUE((*child_instance)->CreateTransform({{.transform_id = kRootTransform}}).is_ok());
    EXPECT_TRUE((*child_instance)->SetRootTransform({{.transform_id = kRootTransform}}).is_ok());
    BlockingPresent(this, *child_instance);

    return child_view_ref;
  }

  std::array<std::array<float, 2>, 2> FullScreenExtents() const {
    return {{{0, 0}, {display_width_, display_height_}}};
  }

  std::unique_ptr<FlatlandClientWithEventHandler> root_instance_;
  fuv::ViewRef root_view_ref_;
  fidl::Client<fupa::LocalHit> local_hit_registry_;
  float display_width_ = 0;
  float display_height_ = 0;

 private:
  fidl::SyncClient<fupi::Registry> pointerinjector_registry_;
  std::unique_ptr<SimpleWatcherClient<fupi::Device>> injector_;

  // Holds watch loops so they stay alive through the duration of the test.
  std::vector<std::function<void(std::vector<fupa::TouchEventWithLocalHit>)>> watch_loops_;
  std::vector<std::function<void(std::vector<fup::TouchEvent>)>> touch_watch_loops_;
};

// In this test we set up three views beneath the root, 1 and its two children 2 and 3, giving us
// the following topology:
//    1
//   / \
//  2   3
// Each view is sized as follows:
// - view 1 is 7x7 pixels
// - view 2 is 3x3 pixels
// - view 3 is 3x3 pixels
// Each view has a default hit region covering their entire View.
// Each view is placed as follows: View 3 is placed above View 2, itself above View 1.
//
// To exercise hit testing, the test performs a touch gesture diagonally across all views and
// observe that the expected local hits are delivered. The display is much larger than view 1, and
// we only exercise the top 8x8 region of the display.
//
// 1: View 1, 2: View 2, 3: View 3, x: No view, []: touch point
//
//   X ->
// Y [1] 1  1  1  1  1  1  x
// |  1 [2] 2  2  1  1  1  x
// v  1  2 [2] 2  1  1  1  x
//    1  2  2 [3] 3  3  1  x
//    1  1  1  3 [3] 3  1  x
//    1  1  1  3  3 [3] 1  x
//    1  1  1  1  1  1 [1] x
//    x  x  x  x  x  x  x [x]
//
TEST_F(FlatlandTouchLocalHitIntegrationTest, InjectedInput_ShouldBeCorrectlyTransformed) {
  // Create View 1
  std::unique_ptr<FlatlandClientWithEventHandler> view1_instance;
  auto [view1_touch_source_client_end, view1_touch_source_server_end] =
      fidl::CreateEndpoints<fup::TouchSource>().value();

  const auto view1_ref =
      CreateAndAddChildView(*root_instance_, /*viewport_transform_id*/ fuc::TransformId(2),
                            /*viewport_spec*/ fuchsia_math::Rect(0, 0, 7, 7),
                            /*parent_of_viewport_transform*/ kRootTransform,
                            /*viewport_content_id*/ fuc::ContentId(2), view1_instance,
                            std::move(view1_touch_source_server_end));
  const auto view1_koid = ExtractKoid(view1_ref);

  // Create View 2
  std::unique_ptr<FlatlandClientWithEventHandler> view2_instance;
  const auto view2_ref =
      CreateAndAddChildView(*view1_instance, /*viewport_transform_id*/ fuc::TransformId(2),
                            /*viewport_spec*/ fuchsia_math::Rect(1, 1, 3, 3),
                            /*parent_of_viewport_transform*/ kRootTransform,
                            /*viewport_content_id*/ fuc::ContentId(2), view2_instance);
  const auto view2_koid = ExtractKoid(view2_ref);

  // Create View 3
  std::unique_ptr<FlatlandClientWithEventHandler> view3_instance;
  const auto view3_ref =
      CreateAndAddChildView(*view1_instance, /*viewport_transform_id*/ fuc::TransformId(3),
                            /*viewport_spec*/ fuchsia_math::Rect(3, 3, 3, 3),
                            /*parent_of_viewport_transform*/ kRootTransform,
                            /*viewport_content_id*/ fuc::ContentId(3), view3_instance);
  const auto view3_koid = ExtractKoid(view3_ref);

  BlockingPresent(this, *view1_instance);
  BlockingPresent(this, *view2_instance);
  BlockingPresent(this, *view3_instance);

  // Upgrade View 1's touch source
  auto touch_source_with_local_hit = UpgradeTouchSource(std::move(view1_touch_source_client_end));
  ASSERT_NE(touch_source_with_local_hit, nullptr);

  std::vector<fupa::TouchEventWithLocalHit> child_events;
  StartWatchLoop(*touch_source_with_local_hit, child_events);

  // Begin test.
  RegisterInjector(scenic::cpp::CloneViewRef(root_view_ref_), scenic::cpp::CloneViewRef(view1_ref));
  Inject(0.5f, 0.5f, fupi::EventPhase::kAdd);
  Inject(1.5f, 1.5f, fupi::EventPhase::kChange);
  Inject(2.5f, 2.5f, fupi::EventPhase::kChange);
  Inject(3.5f, 3.5f, fupi::EventPhase::kChange);
  Inject(4.5f, 4.5f, fupi::EventPhase::kChange);
  Inject(5.5f, 5.5f, fupi::EventPhase::kChange);
  Inject(6.5f, 6.5f, fupi::EventPhase::kChange);
  Inject(7.5f, 7.5f, fupi::EventPhase::kRemove);
  RunLoopUntil([&child_events] { return child_events.size() == 8u; });  // Succeeds or times out.

  EXPECT_EQ(child_events.at(0).local_viewref_koid(), view1_koid);
  EXPECT_EQ(child_events.at(1).local_viewref_koid(), view2_koid);
  EXPECT_EQ(child_events.at(2).local_viewref_koid(), view2_koid);
  EXPECT_EQ(child_events.at(3).local_viewref_koid(), view3_koid);
  EXPECT_EQ(child_events.at(4).local_viewref_koid(), view3_koid);
  EXPECT_EQ(child_events.at(5).local_viewref_koid(), view3_koid);
  EXPECT_EQ(child_events.at(6).local_viewref_koid(), view1_koid);
  EXPECT_EQ(child_events.at(7).local_viewref_koid(), ZX_KOID_INVALID);  // No View

  EXPECT_EQ(child_events.at(0).local_point()[0], 0.5f);  // View 1
  EXPECT_EQ(child_events.at(1).local_point()[0], 0.5f);  // View 2
  EXPECT_EQ(child_events.at(2).local_point()[0], 1.5f);  // View 2
  EXPECT_EQ(child_events.at(3).local_point()[0], 0.5f);  // View 3
  EXPECT_EQ(child_events.at(4).local_point()[0], 1.5f);  // View 3
  EXPECT_EQ(child_events.at(5).local_point()[0], 2.5f);  // View 3
  EXPECT_EQ(child_events.at(6).local_point()[0], 6.5f);  // View 1
  EXPECT_EQ(child_events.at(7).local_point()[0], 0.0f);  // No View
}

// Verifies that when an embedded child View sets its root transform to `kSemanticallyInvisible`,
// `TouchSourceWithLocalHit::Watch()` reports `ZX_KOID_INVALID` (rather than exposing the parent
// View) while regular `TouchSource` dispatch still delivers the touch stream to the child View.
TEST_F(FlatlandTouchLocalHitIntegrationTest,
       SemanticallyInvisibleChild_DoesNotExposeParentInLocalHit) {
  // Declare event vectors before bound clients so `StartWatchLoop()` references outlive them.
  std::vector<fupa::TouchEventWithLocalHit> parent_events;
  std::vector<fup::TouchEvent> child_events;

  std::unique_ptr<FlatlandClientWithEventHandler> parent_instance;
  auto [parent_touch_source_client_end, parent_touch_source_server_end] =
      fidl::CreateEndpoints<fup::TouchSource>().value();
  const auto parent_ref =
      CreateAndAddChildView(*root_instance_, /*viewport_transform_id=*/fuc::TransformId(2),
                            /*viewport_spec=*/fuchsia_math::Rect(0, 0, 7, 7),
                            /*parent_of_viewport_transform=*/kRootTransform,
                            /*viewport_content_id=*/fuc::ContentId(2), parent_instance,
                            std::move(parent_touch_source_server_end));
  const zx_koid_t parent_koid = ExtractKoid(parent_ref);

  std::unique_ptr<FlatlandClientWithEventHandler> child_instance;
  auto [child_touch_source_client_end, child_touch_source_server_end] =
      fidl::CreateEndpoints<fup::TouchSource>().value();
  SimpleWatcherClient<fup::TouchSource> child_touch_source(
      std::move(child_touch_source_client_end), dispatcher(), FailOnClose("Touch source closed"));
  CreateAndAddChildView(*parent_instance, /*viewport_transform_id=*/fuc::TransformId(2),
                        /*viewport_spec=*/fuchsia_math::Rect(1, 1, 3, 3),
                        /*parent_of_viewport_transform=*/kRootTransform,
                        /*viewport_content_id=*/fuc::ContentId(2), child_instance,
                        std::move(child_touch_source_server_end));

  ASSERT_TRUE(
      (*child_instance)
          ->SetInfiniteHitRegion({{.transform_id = kRootTransform,
                                   .hit_test = fuc::HitTestInteraction::kSemanticallyInvisible}})
          .is_ok());
  BlockingPresent(this, *child_instance);
  // `SetInfiniteHitRegion()` does not set `Flatland::view_tree_dirty_`, so a leaf session
  // (`links_to_children_.empty()`) presenting alone does not recompute the `ViewTree` snapshot;
  // present `parent_instance` to trigger recomputation.
  BlockingPresent(this, *parent_instance);

  auto parent_touch_source = UpgradeTouchSource(std::move(parent_touch_source_client_end));
  ASSERT_NE(parent_touch_source, nullptr);

  StartWatchLoop(*parent_touch_source, parent_events, fup::TouchResponseType::kMaybe);
  StartWatchLoop(child_touch_source, child_events, fup::TouchResponseType::kYes);

  RegisterInjector(scenic::cpp::CloneViewRef(root_view_ref_),
                   scenic::cpp::CloneViewRef(parent_ref));

  // Stream 1: Inside the semantically invisible child's viewport `[1..4, 1..4]`.
  Inject(2.5f, 2.5f, fupi::EventPhase::kAdd);
  RunLoopUntil([&parent_events, &child_events] {
    return parent_events.size() >= 2u && child_events.size() >= 2u;
  });
  ASSERT_EQ(parent_events.size(), 2u);
  ASSERT_EQ(child_events.size(), 2u);

  ASSERT_TRUE(parent_events.at(0).touch_event().pointer_sample().has_value());
  EXPECT_EQ(parent_events.at(0).touch_event().pointer_sample()->phase(), fup::EventPhase::kAdd);
  EXPECT_EQ(parent_events.at(0).local_viewref_koid(), ZX_KOID_INVALID);
  EXPECT_EQ(parent_events.at(0).local_point()[0], 0.0f);
  EXPECT_EQ(parent_events.at(0).local_point()[1], 0.0f);
  ASSERT_TRUE(parent_events.at(1).touch_event().interaction_result().has_value());
  EXPECT_EQ(parent_events.at(1).touch_event().interaction_result()->status(),
            fup::TouchInteractionStatus::kDenied);

  ASSERT_TRUE(child_events.at(0).pointer_sample().has_value());
  EXPECT_EQ(child_events.at(0).pointer_sample()->phase(), fup::EventPhase::kAdd);
  // Check `kGranted` before injecting `kRemove` because `TouchSourceBase` sends no `kRemove` to a
  // `kDenied` contender.
  ASSERT_TRUE(child_events.at(1).interaction_result().has_value());
  ASSERT_EQ(child_events.at(1).interaction_result()->status(),
            fup::TouchInteractionStatus::kGranted);

  Inject(2.5f, 2.5f, fupi::EventPhase::kRemove);
  RunLoopUntil([&child_events] { return child_events.size() >= 3u; });
  ASSERT_EQ(child_events.size(), 3u);

  ASSERT_TRUE(child_events.at(2).pointer_sample().has_value());
  EXPECT_EQ(child_events.at(2).pointer_sample()->phase(), fup::EventPhase::kRemove);
  EXPECT_EQ(parent_events.size(), 2u);

  // Stream 2: Outside the child's viewport, inside the parent View `[0..7, 0..7]`.
  Inject(5.5f, 5.5f, fupi::EventPhase::kAdd);
  Inject(5.5f, 5.5f, fupi::EventPhase::kRemove);
  RunLoopUntil([&parent_events] { return parent_events.size() >= 4u; });
  ASSERT_EQ(parent_events.size(), 4u);

  EXPECT_EQ(parent_events.at(2).local_viewref_koid(), parent_koid);
  EXPECT_EQ(parent_events.at(2).local_point()[0], 5.5f);
  EXPECT_EQ(parent_events.at(2).local_point()[1], 5.5f);
  EXPECT_EQ(parent_events.at(3).local_viewref_koid(), parent_koid);
  EXPECT_EQ(parent_events.at(3).local_point()[0], 5.5f);
  EXPECT_EQ(parent_events.at(3).local_point()[1], 5.5f);
  EXPECT_EQ(child_events.size(), 3u);
}

// Verifies that when a parent View places a semantically invisible shield transform above an
// embedded child View, `TouchSourceWithLocalHit::Watch()` passes through the shield to report the
// child View (and the parent's lower transform outside the child), while the parent's shield still
// intercepts regular `TouchSource` dispatch.
TEST_F(FlatlandTouchLocalHitIntegrationTest,
       SemanticallyInvisibleParentShield_ExposesEmbeddedChildInLocalHit) {
  // Declare event vectors before bound clients so `StartWatchLoop()` references outlive them.
  std::vector<fupa::TouchEventWithLocalHit> observer_events;
  std::vector<fup::TouchEvent> parent_events;
  std::vector<fup::TouchEvent> child_events;

  std::unique_ptr<FlatlandClientWithEventHandler> observer_instance;
  auto [observer_touch_source_client_end, observer_touch_source_server_end] =
      fidl::CreateEndpoints<fup::TouchSource>().value();
  const auto observer_ref =
      CreateAndAddChildView(*root_instance_, /*viewport_transform_id=*/fuc::TransformId(2),
                            /*viewport_spec=*/fuchsia_math::Rect(0, 0, 7, 7),
                            /*parent_of_viewport_transform=*/kRootTransform,
                            /*viewport_content_id=*/fuc::ContentId(2), observer_instance,
                            std::move(observer_touch_source_server_end));

  std::unique_ptr<FlatlandClientWithEventHandler> parent_instance;
  auto [parent_touch_source_client_end, parent_touch_source_server_end] =
      fidl::CreateEndpoints<fup::TouchSource>().value();
  SimpleWatcherClient<fup::TouchSource> parent_touch_source(
      std::move(parent_touch_source_client_end), dispatcher(), FailOnClose("Touch source closed"));
  const auto parent_ref =
      CreateAndAddChildView(*observer_instance, /*viewport_transform_id=*/fuc::TransformId(2),
                            /*viewport_spec=*/fuchsia_math::Rect(0, 0, 7, 7),
                            /*parent_of_viewport_transform=*/kRootTransform,
                            /*viewport_content_id=*/fuc::ContentId(2), parent_instance,
                            std::move(parent_touch_source_server_end));
  const zx_koid_t parent_koid = ExtractKoid(parent_ref);

  std::unique_ptr<FlatlandClientWithEventHandler> child_instance;
  auto [child_touch_source_client_end, child_touch_source_server_end] =
      fidl::CreateEndpoints<fup::TouchSource>().value();
  SimpleWatcherClient<fup::TouchSource> child_touch_source(
      std::move(child_touch_source_client_end), dispatcher(), FailOnClose("Touch source closed"));
  const auto child_ref =
      CreateAndAddChildView(*parent_instance, /*viewport_transform_id=*/fuc::TransformId(2),
                            /*viewport_spec=*/fuchsia_math::Rect(1, 1, 3, 3),
                            /*parent_of_viewport_transform=*/kRootTransform,
                            /*viewport_content_id=*/fuc::ContentId(2), child_instance,
                            std::move(child_touch_source_server_end));
  const zx_koid_t child_koid = ExtractKoid(child_ref);

  // Attach `kShieldTransform` after the child's viewport transform so it sits on top of the child
  // View in `parent_instance`'s subtree.
  const fuc::TransformId kShieldTransform(3);
  ASSERT_TRUE((*parent_instance)->CreateTransform({{.transform_id = kShieldTransform}}).is_ok());
  ASSERT_TRUE((*parent_instance)
                  ->AddChild({{.parent_transform_id = kRootTransform,
                               .child_transform_id = kShieldTransform}})
                  .is_ok());
  ASSERT_TRUE(
      (*parent_instance)
          ->SetInfiniteHitRegion({{.transform_id = kShieldTransform,
                                   .hit_test = fuc::HitTestInteraction::kSemanticallyInvisible}})
          .is_ok());
  BlockingPresent(this, *parent_instance);
  BlockingPresent(this, *child_instance);
  BlockingPresent(this, *observer_instance);

  auto observer_touch_source = UpgradeTouchSource(std::move(observer_touch_source_client_end));
  ASSERT_NE(observer_touch_source, nullptr);

  StartWatchLoop(*observer_touch_source, observer_events, fup::TouchResponseType::kMaybe);
  StartWatchLoop(parent_touch_source, parent_events, fup::TouchResponseType::kYes);
  StartWatchLoop(child_touch_source, child_events, fup::TouchResponseType::kYes);

  RegisterInjector(scenic::cpp::CloneViewRef(root_view_ref_),
                   scenic::cpp::CloneViewRef(observer_ref));

  // Stream 1: Over the embedded child View `[1..4, 1..4]` (translated by `(1, 1)`).
  Inject(2.5f, 2.5f, fupi::EventPhase::kAdd);
  RunLoopUntil([&observer_events, &parent_events] {
    return observer_events.size() >= 2u && parent_events.size() >= 2u;
  });
  ASSERT_EQ(observer_events.size(), 2u);
  ASSERT_EQ(parent_events.size(), 2u);

  EXPECT_EQ(observer_events.at(0).local_viewref_koid(), child_koid);
  EXPECT_EQ(observer_events.at(0).local_point()[0], 1.5f);
  EXPECT_EQ(observer_events.at(0).local_point()[1], 1.5f);
  // Check `kGranted` before injecting `kRemove` because `TouchSourceBase` sends no `kRemove` to a
  // `kDenied` contender.
  ASSERT_TRUE(parent_events.at(1).interaction_result().has_value());
  ASSERT_EQ(parent_events.at(1).interaction_result()->status(),
            fup::TouchInteractionStatus::kGranted);

  Inject(2.5f, 2.5f, fupi::EventPhase::kRemove);
  RunLoopUntil([&parent_events] { return parent_events.size() >= 3u; });
  ASSERT_EQ(parent_events.size(), 3u);

  ASSERT_TRUE(parent_events.at(2).pointer_sample().has_value());
  EXPECT_EQ(parent_events.at(2).pointer_sample()->phase(), fup::EventPhase::kRemove);
  EXPECT_TRUE(child_events.empty());

  // Stream 2: Outside the child View, over the parent View `[0..7, 0..7]`.
  Inject(5.5f, 5.5f, fupi::EventPhase::kAdd);
  RunLoopUntil([&observer_events, &parent_events] {
    return observer_events.size() >= 4u && parent_events.size() >= 5u;
  });
  ASSERT_EQ(observer_events.size(), 4u);
  ASSERT_EQ(parent_events.size(), 5u);

  EXPECT_EQ(observer_events.at(2).local_viewref_koid(), parent_koid);
  EXPECT_EQ(observer_events.at(2).local_point()[0], 5.5f);
  EXPECT_EQ(observer_events.at(2).local_point()[1], 5.5f);
  ASSERT_TRUE(parent_events.at(4).interaction_result().has_value());
  ASSERT_EQ(parent_events.at(4).interaction_result()->status(),
            fup::TouchInteractionStatus::kGranted);

  Inject(5.5f, 5.5f, fupi::EventPhase::kRemove);
  RunLoopUntil([&parent_events] { return parent_events.size() >= 6u; });
  ASSERT_EQ(parent_events.size(), 6u);

  ASSERT_TRUE(parent_events.at(5).pointer_sample().has_value());
  EXPECT_EQ(parent_events.at(5).pointer_sample()->phase(), fup::EventPhase::kRemove);
  EXPECT_TRUE(child_events.empty());
}

}  // namespace integration_tests
