// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.ui.pointer/cpp/fidl.h>
#include <fidl/fuchsia.ui.pointerinjector/cpp/fidl.h>
#include <lib/async-testing/test_loop.h>
#include <lib/sys/cpp/testing/component_context_provider.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/ui/scenic/cpp/view_ref_pair.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/lib/testing/loop_fixture/test_loop_fixture.h"
#include "src/ui/scenic/lib/input/input_system.h"
#include "src/ui/scenic/lib/input/touch_source.h"
#include "src/ui/scenic/lib/utils/check_is_on_thread.h"
#include "src/ui/scenic/lib/utils/helpers.h"

// These tests exercise input event delivery under different dispatch policies.
// Setup:
// - Injection done in context View Space, with fuchsia.ui.pointerinjector
// - Target(s) specified by ViewRef
// - Dispatch done in fuchsia.ui.pointer

// All tests in this suite uses the following ViewTree layout:
//  Root
//    |
// Client1
//    |
// Client2
//    |     \
// Client4 Client3

namespace input::test {

using scenic_impl::input::Phase;
using scenic_impl::input::TouchSource;

constexpr float kDisplayWidth = 9.f;
constexpr float kDisplayHeight = 9.f;

class DispatchPolicyTest : public gtest::TestLoopFixture {
 public:
  DispatchPolicyTest()
      : dispatcher_setter_(dispatcher(), dispatcher()),
        snapshot_holder_(std::make_shared<view_tree::SnapshotHolder>()),
        input_system_(dispatcher(), snapshot_holder_, inspect_node_,
                      /*request_focus*/ [](auto...) {}) {}

  void SetUp() override {
    ::testing::Test::SetUp();
    root_vrp_ = scenic::cpp::ViewRefPair::New();
    client1_vrp_ = scenic::cpp::ViewRefPair::New();
    client2_vrp_ = scenic::cpp::ViewRefPair::New();
    client3_vrp_ = scenic::cpp::ViewRefPair::New();
    client4_vrp_ = scenic::cpp::ViewRefPair::New();

    auto [client1_client_end, client1_server_end] =
        fidl::Endpoints<fuchsia_ui_pointer::TouchSource>::Create();
    auto [client2_client_end, client2_server_end] =
        fidl::Endpoints<fuchsia_ui_pointer::TouchSource>::Create();
    auto [client3_client_end, client3_server_end] =
        fidl::Endpoints<fuchsia_ui_pointer::TouchSource>::Create();
    auto [client4_client_end, client4_server_end] =
        fidl::Endpoints<fuchsia_ui_pointer::TouchSource>::Create();

    client1_ptr_.Bind(std::move(client1_client_end), dispatcher(), &client1_event_handler_);
    client2_ptr_.Bind(std::move(client2_client_end), dispatcher(), &client2_event_handler_);
    client3_ptr_.Bind(std::move(client3_client_end), dispatcher(), &client3_event_handler_);
    client4_ptr_.Bind(std::move(client4_client_end), dispatcher(), &client4_event_handler_);

    input_system_.RegisterTouchSource(std::move(client1_server_end), Client1Koid());
    input_system_.RegisterTouchSource(std::move(client2_server_end), Client2Koid());
    input_system_.RegisterTouchSource(std::move(client3_server_end), Client3Koid());
    input_system_.RegisterTouchSource(std::move(client4_server_end), Client4Koid());

    auto [client_end, server_end] = fidl::Endpoints<fuchsia_ui_pointerinjector::Registry>::Create();
    input_system_.BindPointerinjectorRegistry(std::move(server_end));
    registry_client_.Bind(std::move(client_end), dispatcher());
  }

  void RegisterInjector(fuchsia_ui_views::ViewRef context_view_ref,
                        fuchsia_ui_views::ViewRef target_view_ref,
                        fuchsia_ui_pointerinjector::DispatchPolicy dispatch_policy,
                        fuchsia_ui_pointerinjector::DeviceType type) {
    fuchsia_ui_pointerinjector::Config config;
    config.device_id(1);
    config.device_type(type);
    config.dispatch_policy(dispatch_policy);
    {
      fuchsia_ui_pointerinjector::Viewport viewport;
      viewport.extents(
          std::array<std::array<float, 2>, 2>{{{0.f, 0.f}, {kDisplayWidth, kDisplayHeight}}});
      viewport.viewport_to_context_transform(std::array<float, 9>{
          // clang-format off
          1.f, 0.f, 0.f, // first column
          0.f, 1.f, 0.f, // second column
          0.f, 0.f, 1.f, // third column
          // clang-format on
      });
      config.viewport(std::move(viewport));
    }
    config.context(fuchsia_ui_pointerinjector::Context::WithView(std::move(context_view_ref)));
    config.target(fuchsia_ui_pointerinjector::Target::WithView(std::move(target_view_ref)));

    auto [injector_client_end, injector_server_end] =
        fidl::Endpoints<fuchsia_ui_pointerinjector::Device>::Create();
    injector_.Bind(std::move(injector_client_end), dispatcher(), &device_event_handler_);
    bool register_callback_fired = false;
    registry_client_->Register({std::move(config), std::move(injector_server_end)})
        .Then([&register_callback_fired](auto& result) {
          ASSERT_TRUE(result.is_ok());
          register_callback_fired = true;
        });
    RunLoopUntilIdle();
    ASSERT_TRUE(register_callback_fired);
    ASSERT_FALSE(device_event_handler_.error_fired);
  }

  // Creates a new snapshot with a hit test that returns |hits|, and a ViewTree with layout:
  // Root
  //   |
  // Client1
  //   |
  // Client2
  //   |  \
  // Client4 Client3
  std::shared_ptr<view_tree::Snapshot> NewSnapshot(std::vector<zx_koid_t> hits) {
    auto snapshot = std::make_shared<view_tree::Snapshot>();
    snapshot->sequence_number = next_sequence_number_++;
    auto& [root, view_tree, _1, _2, _3] = *snapshot;
    root = RootKoid();
    view_tree[RootKoid()] = {.children = {Client1Koid()}};
    view_tree[Client1Koid()] = {.parent = RootKoid(), .children = {Client2Koid()}};
    view_tree[Client2Koid()] = {.parent = Client1Koid(),
                                .children = {Client3Koid(), Client4Koid()}};
    view_tree[Client3Koid()] = {.parent = Client2Koid()};
    view_tree[Client4Koid()] = {.parent = Client2Koid()};

    snapshot->hit_testers.emplace_back([hits = std::move(hits)](auto...) {
      return view_tree::SubtreeHitTestResult{.hits = hits};
    });

    return snapshot;
  }

  fuchsia_ui_views::ViewRef RootViewRef() { return scenic::cpp::CloneViewRef(root_vrp_.view_ref); }
  fuchsia_ui_views::ViewRef Client1ViewRef() {
    return scenic::cpp::CloneViewRef(client1_vrp_.view_ref);
  }
  fuchsia_ui_views::ViewRef Client2ViewRef() {
    return scenic::cpp::CloneViewRef(client2_vrp_.view_ref);
  }
  fuchsia_ui_views::ViewRef Client3ViewRef() {
    return scenic::cpp::CloneViewRef(client3_vrp_.view_ref);
  }
  fuchsia_ui_views::ViewRef Client4ViewRef() {
    return scenic::cpp::CloneViewRef(client4_vrp_.view_ref);
  }

  zx_koid_t RootKoid() { return utils::ExtractKoid(root_vrp_.view_ref); }
  zx_koid_t Client1Koid() { return utils::ExtractKoid(client1_vrp_.view_ref); }
  zx_koid_t Client2Koid() { return utils::ExtractKoid(client2_vrp_.view_ref); }
  zx_koid_t Client3Koid() { return utils::ExtractKoid(client3_vrp_.view_ref); }
  zx_koid_t Client4Koid() { return utils::ExtractKoid(client4_vrp_.view_ref); }

 private:
  utils::ScopedThreadDispatcherSetter dispatcher_setter_;
  // Must be initialized before |input_system_|.
  sys::testing::ComponentContextProvider context_provider_;
  inspect::Node inspect_node_;

 protected:
  std::shared_ptr<view_tree::SnapshotHolder> snapshot_holder_;
  uint64_t next_sequence_number_ = 1;
  class DeviceEventHandler : public fidl::AsyncEventHandler<fuchsia_ui_pointerinjector::Device> {
   public:
    bool error_fired = false;
    void on_fidl_error(fidl::UnbindInfo error) override { error_fired = true; }
  };

  class ClientEventHandler : public fidl::AsyncEventHandler<fuchsia_ui_pointer::TouchSource> {
   public:
    explicit ClientEventHandler(std::string name) : name_(std::move(name)) {}
    void on_fidl_error(fidl::UnbindInfo info) override {
      FAIL() << name_ << " closed unexpectedly: " << info.FormatDescription();
    }

   private:
    std::string name_;
  };

  scenic_impl::input::InputSystem input_system_;
  fidl::Client<fuchsia_ui_pointerinjector::Registry> registry_client_;
  DeviceEventHandler device_event_handler_;
  ClientEventHandler client1_event_handler_{"Client1"};
  ClientEventHandler client2_event_handler_{"Client2"};
  ClientEventHandler client3_event_handler_{"Client3"};
  ClientEventHandler client4_event_handler_{"Client4"};
  fidl::Client<fuchsia_ui_pointerinjector::Device> injector_;
  fidl::Client<fuchsia_ui_pointer::TouchSource> client1_ptr_;
  fidl::Client<fuchsia_ui_pointer::TouchSource> client2_ptr_;
  fidl::Client<fuchsia_ui_pointer::TouchSource> client3_ptr_;
  fidl::Client<fuchsia_ui_pointer::TouchSource> client4_ptr_;

 private:
  scenic::cpp::ViewRefPair root_vrp_;
  scenic::cpp::ViewRefPair client1_vrp_;
  scenic::cpp::ViewRefPair client2_vrp_;
  scenic::cpp::ViewRefPair client3_vrp_;
  scenic::cpp::ViewRefPair client4_vrp_;
};

class DispatchPolicyTestP : public DispatchPolicyTest, public testing::WithParamInterface<bool> {
 public:
  bool use_inject_events() const { return GetParam(); }

  void Inject(fuchsia_ui_pointerinjector::EventPhase phase) {
    FX_CHECK(injector_.is_valid());
    std::vector<fuchsia_ui_pointerinjector::Event> events;
    {
      fuchsia_ui_pointerinjector::Event event;
      event.timestamp(0);
      fuchsia_ui_pointerinjector::PointerSample pointer_sample;
      pointer_sample.pointer_id(1);
      pointer_sample.phase(phase);
      pointer_sample.position_in_viewport(
          std::array<float, 2>{kDisplayWidth / 2.f, kDisplayHeight / 2.f});
      event.data(fuchsia_ui_pointerinjector::Data::WithPointerSample(std::move(pointer_sample)));
      events.emplace_back(std::move(event));
    }

    if (use_inject_events()) {
      auto result = injector_->InjectEvents({std::move(events)});
      ASSERT_TRUE(result.is_ok());
      RunLoopUntilIdle();
    } else {
      bool inject_callback_fired = false;
      injector_->Inject({std::move(events)}).Then([&inject_callback_fired](auto& result) {
        ASSERT_TRUE(result.is_ok());
        inject_callback_fired = true;
      });
      RunLoopUntilIdle();
      ASSERT_TRUE(inject_callback_fired);
    }
  }
};

INSTANTIATE_TEST_SUITE_P(DispatchPolicyTest, DispatchPolicyTestP, testing::Bool());

TEST_P(DispatchPolicyTestP, ExclusiveMode_ShouldDeliverTo_OnlyTarget) {
  snapshot_holder_->SetSnapshot(NewSnapshot(/*hits*/ {Client4Koid()}));

  {  // Scene is set up. Inject with Client2 as exclusive target.
    RegisterInjector(
        /*context=*/RootViewRef(),
        /*target=*/Client2ViewRef(), fuchsia_ui_pointerinjector::DispatchPolicy::kExclusiveTarget,
        fuchsia_ui_pointerinjector::DeviceType::kTouch);
    Inject(fuchsia_ui_pointerinjector::EventPhase::kAdd);
    Inject(fuchsia_ui_pointerinjector::EventPhase::kChange);
    Inject(fuchsia_ui_pointerinjector::EventPhase::kRemove);
    RunLoopUntilIdle();
  }

  {
    std::vector<fuchsia_ui_pointer::TouchEvent> events;
    client2_ptr_->Watch({{.responses = {}}})
        .Then([&events](fidl::Result<fuchsia_ui_pointer::TouchSource::Watch>& result) {
          ASSERT_TRUE(result.is_ok()) << result.error_value();
          events = std::move(result->events());
        });
    RunLoopUntilIdle();
    EXPECT_EQ(events.size(), 3u);
  }

  {
    bool client1_callback_fired = false;
    client1_ptr_->Watch({{.responses = {}}}).Then([&client1_callback_fired](auto&) {
      client1_callback_fired = true;
    });
    bool client3_callback_fired = false;
    client3_ptr_->Watch({{.responses = {}}}).Then([&client3_callback_fired](auto&) {
      client3_callback_fired = true;
    });
    bool client4_callback_fired = false;
    client4_ptr_->Watch({{.responses = {}}}).Then([&client4_callback_fired](auto&) {
      client4_callback_fired = true;
    });

    RunLoopUntilIdle();
    EXPECT_FALSE(client1_callback_fired);
    EXPECT_FALSE(client3_callback_fired);
    EXPECT_FALSE(client4_callback_fired);
  }
}

TEST_P(DispatchPolicyTestP, TopHitMode_OnLeafTarget_ShouldDeliverTo_OnlyTarget) {
  snapshot_holder_->SetSnapshot(NewSnapshot(/*hits*/ {Client3Koid()}));

  {  // Inject with Client3 as target. Top hit is Client3.
    RegisterInjector(/*context=*/RootViewRef(),
                     /*target=*/Client3ViewRef(),
                     fuchsia_ui_pointerinjector::DispatchPolicy::kTopHitAndAncestorsInTarget,
                     fuchsia_ui_pointerinjector::DeviceType::kTouch);
    Inject(fuchsia_ui_pointerinjector::EventPhase::kAdd);
    Inject(fuchsia_ui_pointerinjector::EventPhase::kChange);
    Inject(fuchsia_ui_pointerinjector::EventPhase::kRemove);
    RunLoopUntilIdle();
  }

  {  // Target should receive events.
    std::vector<fuchsia_ui_pointer::TouchEvent> events;
    client3_ptr_->Watch({{.responses = {}}})
        .Then([&events](fidl::Result<fuchsia_ui_pointer::TouchSource::Watch>& result) {
          ASSERT_TRUE(result.is_ok()) << result.error_value();
          events = std::move(result->events());
        });
    RunLoopUntilIdle();
    EXPECT_EQ(events.size(), 3u);
  }

  {  // No other client should receive any events.
    bool client1_callback_fired = false;
    client1_ptr_->Watch({{.responses = {}}}).Then([&client1_callback_fired](auto&) {
      client1_callback_fired = true;
    });
    bool client2_callback_fired = false;
    client2_ptr_->Watch({{.responses = {}}}).Then([&client2_callback_fired](auto&) {
      client2_callback_fired = true;
    });
    bool client4_callback_fired = false;
    client4_ptr_->Watch({{.responses = {}}}).Then([&client4_callback_fired](auto&) {
      client4_callback_fired = true;
    });

    RunLoopUntilIdle();
    EXPECT_FALSE(client1_callback_fired);
    EXPECT_FALSE(client2_callback_fired);
    EXPECT_FALSE(client4_callback_fired);
  }
}

TEST_P(DispatchPolicyTestP,
       TopHitMode_OnMidTreeTarget_ShouldDeliverTo_TopHitAndAncestorsUpToTarget) {
  snapshot_holder_->SetSnapshot(NewSnapshot(/*hits*/ {Client4Koid()}));

  {  // Inject with Client2 as target. Top hit is Client4.
    RegisterInjector(/*context=*/RootViewRef(),
                     /*target=*/Client2ViewRef(),
                     fuchsia_ui_pointerinjector::DispatchPolicy::kTopHitAndAncestorsInTarget,
                     fuchsia_ui_pointerinjector::DeviceType::kTouch);
    Inject(fuchsia_ui_pointerinjector::EventPhase::kAdd);
    Inject(fuchsia_ui_pointerinjector::EventPhase::kChange);
    Inject(fuchsia_ui_pointerinjector::EventPhase::kRemove);
    RunLoopUntilIdle();
  }

  {  // Top hit should receive events.
    std::vector<fuchsia_ui_pointer::TouchEvent> events;
    client4_ptr_->Watch({{.responses = {}}})
        .Then([&events](fidl::Result<fuchsia_ui_pointer::TouchSource::Watch>& result) {
          ASSERT_TRUE(result.is_ok()) << result.error_value();
          events = std::move(result->events());
        });
    RunLoopUntilIdle();
    EXPECT_EQ(events.size(), 3u);
  }
  {  // Target should receive events, since it's the only ancestor of top hit.
    std::vector<fuchsia_ui_pointer::TouchEvent> events;
    client2_ptr_->Watch({{.responses = {}}})
        .Then([&events](fidl::Result<fuchsia_ui_pointer::TouchSource::Watch>& result) {
          ASSERT_TRUE(result.is_ok()) << result.error_value();
          events = std::move(result->events());
        });
    RunLoopUntilIdle();
    EXPECT_EQ(events.size(), 3u);
  }

  {  // No other client should receive any events.
    bool client1_callback_fired = false;
    client1_ptr_->Watch({{.responses = {}}}).Then([&client1_callback_fired](auto&) {
      client1_callback_fired = true;
    });
    bool client3_callback_fired = false;
    client3_ptr_->Watch({{.responses = {}}}).Then([&client3_callback_fired](auto&) {
      client3_callback_fired = true;
    });

    RunLoopUntilIdle();
    EXPECT_FALSE(client1_callback_fired);
    EXPECT_FALSE(client3_callback_fired);
  }
}

TEST_P(DispatchPolicyTestP, MultiTouchBatch_TouchSourceV2_DeliversSingleOnTouchEventsMessage) {
  class V2EventHandler : public fidl::AsyncEventHandler<fuchsia_ui_pointer::TouchSourceV2> {
   public:
    void OnTouchEvents(
        fidl::Event<fuchsia_ui_pointer::TouchSourceV2::OnTouchEvents>& event) override {
      ++on_touch_events_count;
      batch_sizes.push_back(event.events().size());
      received_events.insert(received_events.end(), std::make_move_iterator(event.events().begin()),
                             std::make_move_iterator(event.events().end()));
    }
    void on_fidl_error(fidl::UnbindInfo info) override {
      FAIL() << "TouchSourceV2 closed unexpectedly: " << info.FormatDescription();
    }
    void handle_unknown_event(
        fidl::UnknownEventMetadata<fuchsia_ui_pointer::TouchSourceV2> metadata) override {}

    size_t on_touch_events_count = 0;
    std::vector<size_t> batch_sizes;
    std::vector<fuchsia_ui_pointer::TouchEvent> received_events;
  };

  // Add a V2 leaf child under Client2 and make it the top hit.
  auto v2_vrp = scenic::cpp::ViewRefPair::New();
  const zx_koid_t v2_koid = utils::ExtractKoid(v2_vrp.view_ref);
  auto [v2_client_end, v2_server_end] =
      fidl::Endpoints<fuchsia_ui_pointer::TouchSourceV2>::Create();
  V2EventHandler v2_handler;
  fidl::Client<fuchsia_ui_pointer::TouchSourceV2> v2_client(std::move(v2_client_end), dispatcher(),
                                                            &v2_handler);
  input_system_.RegisterTouchSourceV2(std::move(v2_server_end), v2_koid);

  auto snapshot = NewSnapshot(/*hits=*/{v2_koid});
  snapshot->view_tree[Client2Koid()].children.insert(v2_koid);
  snapshot->view_tree[v2_koid] = {.parent = Client2Koid()};
  snapshot_holder_->SetSnapshot(snapshot);

  RegisterInjector(/*context=*/RootViewRef(),
                   /*target=*/scenic::cpp::CloneViewRef(v2_vrp.view_ref),
                   fuchsia_ui_pointerinjector::DispatchPolicy::kTopHitAndAncestorsInTarget,
                   fuchsia_ui_pointerinjector::DeviceType::kTouch);

  auto make_sample = [](uint32_t pointer_id, fuchsia_ui_pointerinjector::EventPhase phase, float x,
                        float y, int64_t timestamp) {
    fuchsia_ui_pointerinjector::PointerSample sample;
    sample.pointer_id(pointer_id);
    sample.phase(phase);
    sample.position_in_viewport(std::array<float, 2>{x, y});
    fuchsia_ui_pointerinjector::Event event;
    event.timestamp(timestamp);
    event.data(fuchsia_ui_pointerinjector::Data::WithPointerSample(std::move(sample)));
    return event;
  };

  auto inject_batch = [this](std::vector<fuchsia_ui_pointerinjector::Event> events) {
    if (use_inject_events()) {
      auto result = injector_->InjectEvents({std::move(events)});
      ASSERT_TRUE(result.is_ok());
      RunLoopUntilIdle();
    } else {
      bool inject_callback_fired = false;
      injector_->Inject({std::move(events)}).Then([&inject_callback_fired](auto& result) {
        ASSERT_TRUE(result.is_ok());
        inject_callback_fired = true;
      });
      RunLoopUntilIdle();
      ASSERT_TRUE(inject_callback_fired);
    }
  };

  // 1) Inject 2-finger ADD scan in one batch -> must arrive in a SINGLE OnTouchEvents message.
  {
    std::vector<fuchsia_ui_pointerinjector::Event> batch;
    batch.push_back(make_sample(1, fuchsia_ui_pointerinjector::EventPhase::kAdd, 2.f, 2.f, 1000));
    batch.push_back(make_sample(2, fuchsia_ui_pointerinjector::EventPhase::kAdd, 6.f, 6.f, 1000));
    inject_batch(std::move(batch));
  }
  EXPECT_EQ(v2_handler.on_touch_events_count, 1u);
  EXPECT_THAT(v2_handler.batch_sizes, testing::ElementsAre(2u));

  // 2) Inject 2-finger CHANGE scan in one batch -> must arrive in a SINGLE OnTouchEvents message.
  {
    std::vector<fuchsia_ui_pointerinjector::Event> batch;
    batch.push_back(
        make_sample(1, fuchsia_ui_pointerinjector::EventPhase::kChange, 3.f, 3.f, 5000));
    batch.push_back(
        make_sample(2, fuchsia_ui_pointerinjector::EventPhase::kChange, 7.f, 7.f, 5000));
    inject_batch(std::move(batch));
  }
  EXPECT_EQ(v2_handler.on_touch_events_count, 2u);
  EXPECT_THAT(v2_handler.batch_sizes, testing::ElementsAre(2u, 2u));

  // 3) Close the injector channel while both fingers are ongoing -> both CANCEL events must arrive
  // in a SINGLE OnTouchEvents message.
  injector_ = {};
  RunLoopUntilIdle();
  EXPECT_EQ(v2_handler.on_touch_events_count, 3u);
  EXPECT_THAT(v2_handler.batch_sizes, testing::ElementsAre(2u, 2u, 2u));
  ASSERT_EQ(v2_handler.received_events.size(), 6u);
  EXPECT_EQ(v2_handler.received_events[4].pointer_sample()->phase(),
            fuchsia_ui_pointer::EventPhase::kCancel);
  EXPECT_EQ(v2_handler.received_events[5].pointer_sample()->phase(),
            fuchsia_ui_pointer::EventPhase::kCancel);
}

TEST_P(DispatchPolicyTestP, MultiTouchBatch_TouchSourceV1_WaitingWatchReceivesEntireBatch) {
  snapshot_holder_->SetSnapshot(NewSnapshot(/*hits=*/{Client3Koid()}));

  RegisterInjector(/*context=*/RootViewRef(),
                   /*target=*/Client3ViewRef(),
                   fuchsia_ui_pointerinjector::DispatchPolicy::kTopHitAndAncestorsInTarget,
                   fuchsia_ui_pointerinjector::DeviceType::kTouch);

  // Park a hanging-get Watch() BEFORE injecting the 2-finger batch.
  std::vector<fuchsia_ui_pointer::TouchEvent> watched_events;
  client3_ptr_->Watch({{.responses = {}}})
      .Then([&watched_events](fidl::Result<fuchsia_ui_pointer::TouchSource::Watch>& result) {
        ASSERT_TRUE(result.is_ok()) << result.error_value();
        watched_events = std::move(result->events());
      });
  RunLoopUntilIdle();
  ASSERT_TRUE(watched_events.empty());

  std::vector<fuchsia_ui_pointerinjector::Event> batch;
  for (uint32_t pointer_id : {1u, 2u}) {
    fuchsia_ui_pointerinjector::PointerSample sample;
    sample.pointer_id(pointer_id);
    sample.phase(fuchsia_ui_pointerinjector::EventPhase::kAdd);
    sample.position_in_viewport(std::array<float, 2>{4.f, 4.f});
    fuchsia_ui_pointerinjector::Event event;
    event.timestamp(1000);
    event.data(fuchsia_ui_pointerinjector::Data::WithPointerSample(std::move(sample)));
    batch.push_back(std::move(event));
  }

  if (use_inject_events()) {
    auto result = injector_->InjectEvents({std::move(batch)});
    ASSERT_TRUE(result.is_ok());
    RunLoopUntilIdle();
  } else {
    bool inject_callback_fired = false;
    injector_->Inject({std::move(batch)}).Then([&inject_callback_fired](auto& result) {
      ASSERT_TRUE(result.is_ok());
      inject_callback_fired = true;
    });
    RunLoopUntilIdle();
    ASSERT_TRUE(inject_callback_fired);
  }

  // Both fingers' ADD events must be returned together in the single parked Watch() reply.
  ASSERT_EQ(watched_events.size(), 2u);
  EXPECT_EQ(watched_events[0].pointer_sample()->interaction()->pointer_id(), 1u);
  EXPECT_EQ(watched_events[1].pointer_sample()->interaction()->pointer_id(), 2u);
}

TEST_P(
    DispatchPolicyTestP,
    MultiTouchBatch_ParentContenderWatchResponses_CoalescesContestResultsIntoSingleOnTouchEvents) {
  class V2EventHandler : public fidl::AsyncEventHandler<fuchsia_ui_pointer::TouchSourceV2> {
   public:
    void OnTouchEvents(
        fidl::Event<fuchsia_ui_pointer::TouchSourceV2::OnTouchEvents>& event) override {
      ++on_touch_events_count;
      batch_sizes.push_back(event.events().size());
      received_events.insert(received_events.end(), std::make_move_iterator(event.events().begin()),
                             std::make_move_iterator(event.events().end()));
    }
    void on_fidl_error(fidl::UnbindInfo info) override {
      FAIL() << "TouchSourceV2 closed unexpectedly: " << info.FormatDescription();
    }
    void handle_unknown_event(
        fidl::UnknownEventMetadata<fuchsia_ui_pointer::TouchSourceV2> metadata) override {}

    size_t on_touch_events_count = 0;
    std::vector<size_t> batch_sizes;
    std::vector<fuchsia_ui_pointer::TouchEvent> received_events;
  };

  // Add a V2 leaf child under Client2 and register injector targeting Client2 so both Client2 (V1)
  // and v2_koid (V2) compete in the GestureArena.
  auto v2_vrp = scenic::cpp::ViewRefPair::New();
  const zx_koid_t v2_koid = utils::ExtractKoid(v2_vrp.view_ref);
  auto [v2_client_end, v2_server_end] =
      fidl::Endpoints<fuchsia_ui_pointer::TouchSourceV2>::Create();
  V2EventHandler v2_handler;
  fidl::Client<fuchsia_ui_pointer::TouchSourceV2> v2_client(std::move(v2_client_end), dispatcher(),
                                                            &v2_handler);
  input_system_.RegisterTouchSourceV2(std::move(v2_server_end), v2_koid);

  auto snapshot = NewSnapshot(/*hits=*/{v2_koid});
  snapshot->view_tree[Client2Koid()].children.insert(v2_koid);
  snapshot->view_tree[v2_koid] = {.parent = Client2Koid()};
  snapshot_holder_->SetSnapshot(snapshot);

  RegisterInjector(/*context=*/RootViewRef(),
                   /*target=*/Client2ViewRef(),
                   fuchsia_ui_pointerinjector::DispatchPolicy::kTopHitAndAncestorsInTarget,
                   fuchsia_ui_pointerinjector::DeviceType::kTouch);

  // Park a Watch() on parent Client2 before injecting 2-finger ADD.
  std::vector<fuchsia_ui_pointer::TouchEvent> client2_events;
  client2_ptr_->Watch({{.responses = {}}})
      .Then([&client2_events](fidl::Result<fuchsia_ui_pointer::TouchSource::Watch>& result) {
        ASSERT_TRUE(result.is_ok()) << result.error_value();
        client2_events = std::move(result->events());
      });
  RunLoopUntilIdle();

  std::vector<fuchsia_ui_pointerinjector::Event> batch;
  for (uint32_t pointer_id : {1u, 2u}) {
    fuchsia_ui_pointerinjector::PointerSample sample;
    sample.pointer_id(pointer_id);
    sample.phase(fuchsia_ui_pointerinjector::EventPhase::kAdd);
    sample.position_in_viewport(std::array<float, 2>{4.f, 4.f});
    fuchsia_ui_pointerinjector::Event event;
    event.timestamp(1000);
    event.data(fuchsia_ui_pointerinjector::Data::WithPointerSample(std::move(sample)));
    batch.push_back(std::move(event));
  }

  if (use_inject_events()) {
    auto result = injector_->InjectEvents({std::move(batch)});
    ASSERT_TRUE(result.is_ok());
    RunLoopUntilIdle();
  } else {
    bool inject_callback_fired = false;
    injector_->Inject({std::move(batch)}).Then([&inject_callback_fired](auto& result) {
      ASSERT_TRUE(result.is_ok());
      inject_callback_fired = true;
    });
    RunLoopUntilIdle();
    ASSERT_TRUE(inject_callback_fired);
  }

  // Both Client2 (V1) and v2_koid (V2) receive both fingers in a single batch.
  ASSERT_EQ(client2_events.size(), 2u);
  EXPECT_EQ(v2_handler.on_touch_events_count, 1u);
  EXPECT_THAT(v2_handler.batch_sizes, testing::ElementsAre(2u));

  // Parent Client2 claims both streams in a single Watch() call -> both DENIED interaction results
  // must arrive at v2_koid in a SINGLE OnTouchEvents message.
  std::vector<fuchsia_ui_pointer::TouchResponse> responses(2);
  responses[0].response_type(fuchsia_ui_pointer::TouchResponseType::kYesPrioritize);
  responses[1].response_type(fuchsia_ui_pointer::TouchResponseType::kYesPrioritize);
  client2_ptr_->Watch({{.responses = std::move(responses)}}).Then([](auto&) {});
  RunLoopUntilIdle();

  EXPECT_EQ(v2_handler.on_touch_events_count, 2u);
  EXPECT_THAT(v2_handler.batch_sizes, testing::ElementsAre(2u, 2u));
  ASSERT_EQ(v2_handler.received_events.size(), 4u);
  ASSERT_TRUE(v2_handler.received_events[2].interaction_result().has_value());
  ASSERT_TRUE(v2_handler.received_events[3].interaction_result().has_value());
  EXPECT_EQ(v2_handler.received_events[2].interaction_result()->status(),
            fuchsia_ui_pointer::TouchInteractionStatus::kDenied);
  EXPECT_EQ(v2_handler.received_events[3].interaction_result()->status(),
            fuchsia_ui_pointer::TouchInteractionStatus::kDenied);
}

TEST_P(DispatchPolicyTestP,
       MultiTouchBatch_ContenderDisconnect_CoalescesContestResultsIntoSingleOnTouchEvents) {
  class V2EventHandler : public fidl::AsyncEventHandler<fuchsia_ui_pointer::TouchSourceV2> {
   public:
    void OnTouchEvents(
        fidl::Event<fuchsia_ui_pointer::TouchSourceV2::OnTouchEvents>& event) override {
      ++on_touch_events_count;
      batch_sizes.push_back(event.events().size());
      received_events.insert(received_events.end(), std::make_move_iterator(event.events().begin()),
                             std::make_move_iterator(event.events().end()));
    }
    void on_fidl_error(fidl::UnbindInfo info) override {
      FAIL() << "TouchSourceV2 closed unexpectedly: " << info.FormatDescription();
    }
    void handle_unknown_event(
        fidl::UnknownEventMetadata<fuchsia_ui_pointer::TouchSourceV2> metadata) override {}

    size_t on_touch_events_count = 0;
    std::vector<size_t> batch_sizes;
    std::vector<fuchsia_ui_pointer::TouchEvent> received_events;
  };

  // Add a V2 leaf child under Client2 and register injector targeting Client2 so both Client2 (V1)
  // and v2_koid (V2) compete in the GestureArena.
  auto v2_vrp = scenic::cpp::ViewRefPair::New();
  const zx_koid_t v2_koid = utils::ExtractKoid(v2_vrp.view_ref);
  auto [v2_client_end, v2_server_end] =
      fidl::Endpoints<fuchsia_ui_pointer::TouchSourceV2>::Create();
  V2EventHandler v2_handler;
  fidl::Client<fuchsia_ui_pointer::TouchSourceV2> v2_client(std::move(v2_client_end), dispatcher(),
                                                            &v2_handler);
  input_system_.RegisterTouchSourceV2(std::move(v2_server_end), v2_koid);

  auto snapshot = NewSnapshot(/*hits=*/{v2_koid});
  snapshot->view_tree[Client2Koid()].children.insert(v2_koid);
  snapshot->view_tree[v2_koid] = {.parent = Client2Koid()};
  snapshot_holder_->SetSnapshot(snapshot);

  RegisterInjector(/*context=*/RootViewRef(),
                   /*target=*/Client2ViewRef(),
                   fuchsia_ui_pointerinjector::DispatchPolicy::kTopHitAndAncestorsInTarget,
                   fuchsia_ui_pointerinjector::DeviceType::kTouch);

  // Park a Watch() on parent Client2 before injecting 2-finger ADD.
  std::vector<fuchsia_ui_pointer::TouchEvent> client2_events;
  client2_ptr_->Watch({{.responses = {}}})
      .Then([&client2_events](fidl::Result<fuchsia_ui_pointer::TouchSource::Watch>& result) {
        ASSERT_TRUE(result.is_ok()) << result.error_value();
        client2_events = std::move(result->events());
      });
  RunLoopUntilIdle();

  std::vector<fuchsia_ui_pointerinjector::Event> batch;
  for (uint32_t pointer_id : {1u, 2u}) {
    fuchsia_ui_pointerinjector::PointerSample sample;
    sample.pointer_id(pointer_id);
    sample.phase(fuchsia_ui_pointerinjector::EventPhase::kAdd);
    sample.position_in_viewport(std::array<float, 2>{4.f, 4.f});
    fuchsia_ui_pointerinjector::Event event;
    event.timestamp(1000);
    event.data(fuchsia_ui_pointerinjector::Data::WithPointerSample(std::move(sample)));
    batch.push_back(std::move(event));
  }

  if (use_inject_events()) {
    auto result = injector_->InjectEvents({std::move(batch)});
    ASSERT_TRUE(result.is_ok());
    RunLoopUntilIdle();
  } else {
    bool inject_callback_fired = false;
    injector_->Inject({std::move(batch)}).Then([&inject_callback_fired](auto& result) {
      ASSERT_TRUE(result.is_ok());
      inject_callback_fired = true;
    });
    RunLoopUntilIdle();
    ASSERT_TRUE(inject_callback_fired);
  }

  // Both Client2 (V1) and v2_koid (V2) receive both fingers in a single batch.
  ASSERT_EQ(client2_events.size(), 2u);
  EXPECT_EQ(v2_handler.on_touch_events_count, 1u);
  EXPECT_THAT(v2_handler.batch_sizes, testing::ElementsAre(2u));

  // Disconnect Client2 -> EraseContender resolves both streams and awards the win to v2_koid.
  // Both GRANTED interaction results must arrive at v2_koid in a SINGLE OnTouchEvents message.
  client2_ptr_ = {};
  RunLoopUntilIdle();

  EXPECT_EQ(v2_handler.on_touch_events_count, 2u);
  EXPECT_THAT(v2_handler.batch_sizes, testing::ElementsAre(2u, 2u));
  ASSERT_EQ(v2_handler.received_events.size(), 4u);
  ASSERT_TRUE(v2_handler.received_events[2].interaction_result().has_value());
  ASSERT_TRUE(v2_handler.received_events[3].interaction_result().has_value());
  EXPECT_EQ(v2_handler.received_events[2].interaction_result()->status(),
            fuchsia_ui_pointer::TouchInteractionStatus::kGranted);
  EXPECT_EQ(v2_handler.received_events[3].interaction_result()->status(),
            fuchsia_ui_pointer::TouchInteractionStatus::kGranted);
}

}  // namespace input::test
