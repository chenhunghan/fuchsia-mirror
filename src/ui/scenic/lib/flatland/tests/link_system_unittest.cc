// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/link_system.h"

#include <fidl/fuchsia.ui.composition/cpp/fidl.h>
#include <fidl/fuchsia.ui.views/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/ui/scenic/cpp/view_creation_tokens.h>
#include <lib/ui/scenic/cpp/view_identity.h>

#include <memory>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/ui/scenic/lib/flatland/tests/logging_event_loop.h"
#include "src/ui/scenic/lib/flatland/uber_struct_system.h"
#include "src/ui/scenic/lib/utils/dispatcher_holder.h"
#include "src/ui/scenic/lib/utils/pmr_stats_resource.h"

using flatland::LinkSystem;
using LinkToChild = flatland::LinkSystem::LinkToChild;
using LinkToParent = flatland::LinkSystem::LinkToParent;
using flatland::TransformGraph;
using flatland::UberStructSystem;
using TopologyEntry = flatland::TransformGraph::TopologyEntry;
using flatland::TransformHandle;
using fuchsia_math::SizeU;
using fuchsia_ui_composition::ChildViewWatcher;
using fuchsia_ui_composition::LayoutInfo;
using fuchsia_ui_composition::ParentViewportWatcher;
using fuchsia_ui_composition::ViewportProperties;
using fuchsia_ui_views::ViewCreationToken;
using fuchsia_ui_views::ViewportCreationToken;

namespace flatland {
namespace test {

class LinkSystemTest : public LoggingEventLoop, public ::testing::Test {
 public:
  LinkSystemTest()
      : uber_struct_system_(std::make_shared<UberStructSystem>()),
        root_instance_id_(uber_struct_system_->GetNextInstanceId()),
        root_graph_(root_instance_id_),
        root_handle_(root_graph_.CreateTransform()) {}

  std::shared_ptr<LinkSystem> CreateViewportSystem() {
    return std::make_shared<LinkSystem>(uber_struct_system_->GetNextInstanceId());
  }

  TransformGraph CreateTransformGraph() {
    return TransformGraph(uber_struct_system_->GetNextInstanceId());
  }

  void SetUp() override {
    ::testing::Test::SetUp();
    // UnownedDispatcherHolder is safe to use because the dispatcher will be valid until TearDown().
    dispatcher_holder_ = std::make_shared<utils::UnownedDispatcherHolder>(dispatcher());
  }

  void TearDown() override {
    dispatcher_holder_.reset();
    ::testing::Test::TearDown();
  }

  const std::shared_ptr<UberStructSystem> uber_struct_system_;
  const TransformHandle::InstanceId root_instance_id_;
  TransformGraph root_graph_;
  TransformHandle root_handle_;
  std::shared_ptr<utils::DispatcherHolder> dispatcher_holder_;
};

TEST_F(LinkSystemTest, UnresolvedParentViewportWatcherDiesOnContentTokenDeath) {
  auto link_system = CreateViewportSystem();

  auto [child_token, parent_token] = scenic::cpp::ViewCreationTokenPair::New();

  TransformHandle handle;

  auto [client_end, server_end] = fidl::Endpoints<ChildViewWatcher>::Create();
  class EventHandler : public fidl::AsyncEventHandler<ChildViewWatcher> {
   public:
    void on_fidl_error(fidl::UnbindInfo info) override {
      EXPECT_EQ(info.status(), ZX_ERR_PEER_CLOSED);
      closed = true;
    }
    bool closed = false;
  };
  EventHandler event_handler;
  fidl::Client<ChildViewWatcher> child_view_watcher(std::move(client_end), dispatcher(),
                                                    &event_handler);

  ViewportProperties properties;
  properties.logical_size(SizeU{{.width = 1, .height = 2}});
  properties.inset(fuchsia_math::Inset{{.top = 0, .right = 0, .bottom = 0, .left = 0}});
  LinkToChild link_to_child = link_system->CreateLinkToChild(
      dispatcher_holder_, std::move(parent_token), std::move(properties), std::move(server_end),
      handle, [](const std::string& error_log) { GTEST_FAIL() << error_log; });
  EXPECT_TRUE(link_to_child.importer.valid());
  EXPECT_FALSE(event_handler.closed);

  child_token.value().reset();
  RunLoopUntilIdle();

  EXPECT_FALSE(link_to_child.importer.valid());
  EXPECT_TRUE(event_handler.closed);
}

TEST_F(LinkSystemTest, UnresolvedChildViewWatcherDiesOnGraphTokenDeath) {
  auto link_system = CreateViewportSystem();

  auto [child_token, parent_token] = scenic::cpp::ViewCreationTokenPair::New();

  TransformHandle handle;

  auto [client_end, server_end] = fidl::Endpoints<ParentViewportWatcher>::Create();
  class EventHandler : public fidl::AsyncEventHandler<ParentViewportWatcher> {
   public:
    void on_fidl_error(fidl::UnbindInfo info) override {
      EXPECT_EQ(info.status(), ZX_ERR_PEER_CLOSED);
      closed = true;
    }
    bool closed = false;
  };
  EventHandler event_handler;
  fidl::Client<ParentViewportWatcher> parent_viewport_watcher(std::move(client_end), dispatcher(),
                                                              &event_handler);

  LinkToParent link_to_parent = link_system->CreateLinkToParent(
      dispatcher_holder_, std::move(child_token), scenic::cpp::NewViewIdentityOnCreation(),
      std::move(server_end), handle,
      [](const std::string& error_log) { GTEST_FAIL() << error_log; });
  EXPECT_TRUE(link_to_parent.exporter.valid());
  EXPECT_FALSE(event_handler.closed);

  parent_token.value().reset();
  RunLoopUntilIdle();

  EXPECT_FALSE(link_to_parent.exporter.valid());
  EXPECT_TRUE(event_handler.closed);
}

TEST_F(LinkSystemTest, ResolvedLinkCreatesLinkTopology) {
  auto link_system = CreateViewportSystem();
  auto child_graph = CreateTransformGraph();
  auto parent_graph = CreateTransformGraph();

  link_system->UpdateDevicePixelRatio(glm::vec2{2.f, 2.f});

  auto [child_token, parent_token] = scenic::cpp::ViewCreationTokenPair::New();

  auto [parent_client_end, parent_server_end] = fidl::Endpoints<ParentViewportWatcher>::Create();
  fidl::Client<ParentViewportWatcher> parent_viewport_watcher(std::move(parent_client_end),
                                                              dispatcher());
  LinkToParent link_to_parent = link_system->CreateLinkToParent(
      dispatcher_holder_, std::move(child_token), scenic::cpp::NewViewIdentityOnCreation(),
      std::move(parent_server_end), child_graph.CreateTransform(),
      [](const std::string& error_log) { GTEST_FAIL() << error_log; });
  EXPECT_TRUE(link_to_parent.exporter.valid());

  auto [child_client_end, child_server_end] = fidl::Endpoints<ChildViewWatcher>::Create();
  fidl::Client<ChildViewWatcher> child_view_watcher(std::move(child_client_end), dispatcher());
  ViewportProperties properties;
  properties.logical_size(SizeU{{.width = 1, .height = 2}});
  properties.inset(fuchsia_math::Inset{{.top = 0, .right = 0, .bottom = 0, .left = 0}});
  LinkToChild link_to_child = link_system->CreateLinkToChild(
      dispatcher_holder_, std::move(parent_token), std::move(properties),
      std::move(child_server_end), parent_graph.CreateTransform(),
      [](const std::string& error_log) { GTEST_FAIL() << error_log; });

  EXPECT_TRUE(link_to_child.importer.valid());

  GlobalTopologyData::LinkTopologyMap links;
  link_system->GetResolvedTopologyLinks(links);
  EXPECT_FALSE(links.empty());
  EXPECT_TRUE(links.contains(link_to_child.internal_link_handle));
  EXPECT_EQ(links[link_to_child.internal_link_handle], link_to_parent.child_transform_handle);

  bool layout_updated = false;
  parent_viewport_watcher->GetLayout().Then(
      [&](fidl::Result<ParentViewportWatcher::GetLayout>& result) {
        ASSERT_TRUE(result.is_ok());
        const auto& info = result.value().info();
        EXPECT_EQ(1u, info.logical_size()->width());
        EXPECT_EQ(2u, info.logical_size()->height());
        EXPECT_EQ(2.f, info.device_pixel_ratio()->x());
        EXPECT_EQ(2.f, info.device_pixel_ratio()->y());
        layout_updated = true;
      });
  EXPECT_FALSE(layout_updated);
  RunLoopUntilIdle();
  ASSERT_TRUE(layout_updated);
}

TEST_F(LinkSystemTest, LinkToChildDeathDestroysTopology) {
  auto link_system = CreateViewportSystem();
  auto child_graph = CreateTransformGraph();
  auto parent_graph = CreateTransformGraph();

  auto [child_token, parent_token] = scenic::cpp::ViewCreationTokenPair::New();

  auto [parent_client_end, parent_server_end] = fidl::Endpoints<ParentViewportWatcher>::Create();
  fidl::Client<ParentViewportWatcher> parent_viewport_watcher(std::move(parent_client_end),
                                                              dispatcher());
  LinkToParent link_to_parent = link_system->CreateLinkToParent(
      dispatcher_holder_, std::move(child_token), scenic::cpp::NewViewIdentityOnCreation(),
      std::move(parent_server_end), child_graph.CreateTransform(),
      [](const std::string& error_log) { GTEST_FAIL() << error_log; });

  GlobalTopologyData::LinkTopologyMap links;
  {
    auto [child_client_end, child_server_end] = fidl::Endpoints<ChildViewWatcher>::Create();
    fidl::Client<ChildViewWatcher> child_view_watcher(std::move(child_client_end), dispatcher());
    ViewportProperties properties;
    properties.logical_size(SizeU{{.width = 1, .height = 2}});
    properties.inset(fuchsia_math::Inset{{.top = 0, .right = 0, .bottom = 0, .left = 0}});
    LinkToChild link_to_child = link_system->CreateLinkToChild(
        dispatcher_holder_, std::move(parent_token), std::move(properties),
        std::move(child_server_end), parent_graph.CreateTransform(),
        [](const std::string& error_log) { GTEST_FAIL() << error_log; });

    link_system->GetResolvedTopologyLinks(links);
    EXPECT_FALSE(links.empty());
    EXPECT_TRUE(links.contains(link_to_child.internal_link_handle));
    EXPECT_EQ(links[link_to_child.internal_link_handle], link_to_parent.child_transform_handle);

    // |link_to_child| dies here, which destroys the link topology.
  }

  link_system->GetResolvedTopologyLinks(links);
  EXPECT_TRUE(links.empty());
}

TEST_F(LinkSystemTest, LinkToParentDeathDestroysTopology) {
  auto link_system = CreateViewportSystem();
  auto child_graph = CreateTransformGraph();
  auto parent_graph = CreateTransformGraph();

  auto [child_token, parent_token] = scenic::cpp::ViewCreationTokenPair::New();

  auto [child_client_end, child_server_end] = fidl::Endpoints<ChildViewWatcher>::Create();
  fidl::Client<ChildViewWatcher> child_view_watcher(std::move(child_client_end), dispatcher());
  ViewportProperties properties;
  properties.logical_size(SizeU{{.width = 1, .height = 2}});
  properties.inset(fuchsia_math::Inset{{.top = 0, .right = 0, .bottom = 0, .left = 0}});
  LinkToChild link_to_child = link_system->CreateLinkToChild(
      dispatcher_holder_, std::move(parent_token), std::move(properties),
      std::move(child_server_end), parent_graph.CreateTransform(),
      [](const std::string& error_log) { GTEST_FAIL() << error_log; });

  GlobalTopologyData::LinkTopologyMap links;
  {
    auto [parent_client_end, parent_server_end] = fidl::Endpoints<ParentViewportWatcher>::Create();
    fidl::Client<ParentViewportWatcher> parent_viewport_watcher(std::move(parent_client_end),
                                                                dispatcher());
    LinkToParent parent_link = link_system->CreateLinkToParent(
        dispatcher_holder_, std::move(child_token), scenic::cpp::NewViewIdentityOnCreation(),
        std::move(parent_server_end), child_graph.CreateTransform(),
        [](const std::string& error_log) { GTEST_FAIL() << error_log; });

    link_system->GetResolvedTopologyLinks(links);
    EXPECT_FALSE(links.empty());
    EXPECT_TRUE(links.contains(link_to_child.internal_link_handle));
    EXPECT_EQ(links[link_to_child.internal_link_handle], parent_link.child_transform_handle);

    // |parent_link| dies here, which destroys the link topology.
  }

  link_system->GetResolvedTopologyLinks(links);
  EXPECT_TRUE(links.empty());
}

TEST_F(LinkSystemTest, LinkMapsAllocateFromCallerResource) {
  auto link_system = CreateViewportSystem();
  auto child_graph = CreateTransformGraph();
  auto parent_graph = CreateTransformGraph();

  auto [child_token, parent_token] = scenic::cpp::ViewCreationTokenPair::New();

  auto [parent_client_end, parent_server_end] = fidl::Endpoints<ParentViewportWatcher>::Create();
  fidl::Client<ParentViewportWatcher> parent_viewport_watcher(std::move(parent_client_end),
                                                              dispatcher());
  const TransformHandle child_transform = child_graph.CreateTransform();
  LinkToParent link_to_parent = link_system->CreateLinkToParent(
      dispatcher_holder_, std::move(child_token), scenic::cpp::NewViewIdentityOnCreation(),
      std::move(parent_server_end), child_transform,
      [](const std::string& error_log) { GTEST_FAIL() << error_log; });

  auto [child_client_end, child_server_end] = fidl::Endpoints<ChildViewWatcher>::Create();
  fidl::Client<ChildViewWatcher> child_view_watcher(std::move(child_client_end), dispatcher());
  ViewportProperties properties;
  properties.logical_size(SizeU{{.width = 1, .height = 2}});
  properties.inset(fuchsia_math::Inset{{.top = 0, .right = 0, .bottom = 0, .left = 0}});
  const TransformHandle parent_transform = parent_graph.CreateTransform();
  LinkToChild link_to_child = link_system->CreateLinkToChild(
      dispatcher_holder_, std::move(parent_token), std::move(properties),
      std::move(child_server_end), parent_transform,
      [](const std::string& error_log) { GTEST_FAIL() << error_log; });

  {
    utils::PmrStatsResource stats_resource;
    GlobalTopologyData::LinkTopologyMap links(&stats_resource);
    link_system->GetResolvedTopologyLinks(links);
    EXPECT_FALSE(links.empty());
    EXPECT_GT(stats_resource.total_allocation_count(), 0u);
  }

  {
    utils::PmrStatsResource stats_resource;
    GlobalTopologyData::ChildToParentTransformMap child_to_parent_map(&stats_resource);
    link_system->GetLinkChildToParentTransformMap(child_to_parent_map);
    EXPECT_FALSE(child_to_parent_map.empty());
    EXPECT_GT(stats_resource.total_allocation_count(), 0u);
  }
}

TEST_F(LinkSystemTest, ChildToParentTransformMapTracksLinks) {
  auto link_system = CreateViewportSystem();
  auto child_graph = CreateTransformGraph();
  auto parent_graph = CreateTransformGraph();
  const TransformHandle child_transform = child_graph.CreateTransform();
  const TransformHandle parent_transform = parent_graph.CreateTransform();

  GlobalTopologyData::ChildToParentTransformMap child_to_parent_map;

  // Before resolution the map is empty.
  link_system->GetLinkChildToParentTransformMap(child_to_parent_map);
  EXPECT_TRUE(child_to_parent_map.empty());

  auto [child_token, parent_token] = scenic::cpp::ViewCreationTokenPair::New();

  auto [parent_client_end, parent_server_end] = fidl::Endpoints<ParentViewportWatcher>::Create();
  fidl::Client<ParentViewportWatcher> parent_viewport_watcher(std::move(parent_client_end),
                                                              dispatcher());
  LinkToParent link_to_parent = link_system->CreateLinkToParent(
      dispatcher_holder_, std::move(child_token), scenic::cpp::NewViewIdentityOnCreation(),
      std::move(parent_server_end), child_transform,
      [](const std::string& error_log) { GTEST_FAIL() << error_log; });

  {
    auto [child_client_end, child_server_end] = fidl::Endpoints<ChildViewWatcher>::Create();
    fidl::Client<ChildViewWatcher> child_view_watcher(std::move(child_client_end), dispatcher());
    ViewportProperties properties;
    properties.logical_size(SizeU{{.width = 1, .height = 2}});
    properties.inset(fuchsia_math::Inset{{.top = 0, .right = 0, .bottom = 0, .left = 0}});
    LinkToChild link_to_child = link_system->CreateLinkToChild(
        dispatcher_holder_, std::move(parent_token), std::move(properties),
        std::move(child_server_end), parent_transform,
        [](const std::string& error_log) { GTEST_FAIL() << error_log; });

    // After resolution it maps the LinkToParent's child transform to the LinkToChild's parent
    // transform.
    link_system->GetLinkChildToParentTransformMap(child_to_parent_map);
    EXPECT_EQ(child_to_parent_map.size(), 1u);
    EXPECT_EQ(child_to_parent_map[child_transform], parent_transform);

    // |link_to_child| dies here, which invalidates the link.
  }

  // After the link dies, a second call with the same (now non-empty) map leaves it empty.
  link_system->GetLinkChildToParentTransformMap(child_to_parent_map);
  EXPECT_TRUE(child_to_parent_map.empty());
}

// `GetResolvedTopologyLinks()` returns a generation that starts at 1, advances once when a link
// resolves and once when it is invalidated, and is not consumed by reading it (in contrast to
// `UberStructSystem::MustRecomputeSceneState()`).
TEST_F(LinkSystemTest, TopologyGenerationAdvancesOnResolveAndInvalidate) {
  auto link_system = CreateViewportSystem();
  auto child_graph = CreateTransformGraph();
  auto parent_graph = CreateTransformGraph();

  GlobalTopologyData::LinkTopologyMap links;
  EXPECT_EQ(link_system->GetResolvedTopologyLinks(links), 1u);
  EXPECT_TRUE(links.empty());

  auto [child_token, parent_token] = scenic::cpp::ViewCreationTokenPair::New();

  auto [parent_client_end, parent_server_end] = fidl::Endpoints<ParentViewportWatcher>::Create();
  fidl::Client<ParentViewportWatcher> parent_viewport_watcher(std::move(parent_client_end),
                                                              dispatcher());
  LinkToParent link_to_parent = link_system->CreateLinkToParent(
      dispatcher_holder_, std::move(child_token), scenic::cpp::NewViewIdentityOnCreation(),
      std::move(parent_server_end), child_graph.CreateTransform(),
      [](const std::string& error_log) { GTEST_FAIL() << error_log; });

  {
    auto [child_client_end, child_server_end] = fidl::Endpoints<ChildViewWatcher>::Create();
    fidl::Client<ChildViewWatcher> child_view_watcher(std::move(child_client_end), dispatcher());
    ViewportProperties properties;
    properties.logical_size(SizeU{{.width = 1, .height = 2}});
    properties.inset(fuchsia_math::Inset{{.top = 0, .right = 0, .bottom = 0, .left = 0}});
    LinkToChild link_to_child = link_system->CreateLinkToChild(
        dispatcher_holder_, std::move(parent_token), std::move(properties),
        std::move(child_server_end), parent_graph.CreateTransform(),
        [](const std::string& error_log) { GTEST_FAIL() << error_log; });

    // The link resolved.
    EXPECT_EQ(link_system->GetResolvedTopologyLinks(links), 2u);
    EXPECT_EQ(links.size(), 1u);

    // A second read without changes returns the same generation.
    EXPECT_EQ(link_system->GetResolvedTopologyLinks(links), 2u);
    EXPECT_EQ(links.size(), 1u);

    // `link_to_child` dies here, which invalidates the link.
  }

  EXPECT_EQ(link_system->GetResolvedTopologyLinks(links), 3u);
  EXPECT_TRUE(links.empty());
}

// `GetResolvedTopologyLinksIfChanged()` leaves the output map untouched when the caller already
// holds the current generation, and replaces its contents otherwise.
TEST_F(LinkSystemTest, ResolvedTopologyLinksCopiedOnlyWhenGenerationMoved) {
  auto link_system = CreateViewportSystem();
  const TransformHandle kSentinelKey(99, 1);
  const TransformHandle kSentinelValue(99, 2);

  GlobalTopologyData::LinkTopologyMap links;
  links[kSentinelKey] = kSentinelValue;

  // A fresh `LinkSystem` is at generation 1 with no links.
  EXPECT_EQ(link_system->GetResolvedTopologyLinksIfChanged(/*known_generation=*/1, links), 1u);
  ASSERT_EQ(links.size(), 1u);
  EXPECT_EQ(links.at(kSentinelKey), kSentinelValue);

  EXPECT_EQ(link_system->GetResolvedTopologyLinksIfChanged(/*known_generation=*/0, links), 1u);
  EXPECT_TRUE(links.empty());
}

TEST_F(LinkSystemTest, OverwrittenHangingGetsReturnError) {
  auto link_system = CreateViewportSystem();
  auto child_graph = CreateTransformGraph();
  auto parent_graph = CreateTransformGraph();

  auto [child_token, parent_token] = scenic::cpp::ViewCreationTokenPair::New();

  auto [parent_client_end, parent_server_end] = fidl::Endpoints<ParentViewportWatcher>::Create();
  fidl::Client<ParentViewportWatcher> parent_viewport_watcher(std::move(parent_client_end),
                                                              dispatcher());
  bool link_to_parent_returned_error = false;
  LinkToParent link_to_parent = link_system->CreateLinkToParent(
      dispatcher_holder_, std::move(child_token), scenic::cpp::NewViewIdentityOnCreation(),
      std::move(parent_server_end), child_graph.CreateTransform(),
      [&](const std::string& error_log) { link_to_parent_returned_error = true; });

  auto [child_client_end, child_server_end] = fidl::Endpoints<ChildViewWatcher>::Create();
  fidl::Client<ChildViewWatcher> child_view_watcher(std::move(child_client_end), dispatcher());
  bool link_to_child_returned_error = false;
  ViewportProperties properties;
  properties.logical_size(SizeU{{.width = 1, .height = 2}});
  properties.inset(fuchsia_math::Inset{{.top = 0, .right = 0, .bottom = 0, .left = 0}});
  LinkToChild link_to_child = link_system->CreateLinkToChild(
      dispatcher_holder_, std::move(parent_token), std::move(properties),
      std::move(child_server_end), parent_graph.CreateTransform(),
      [&](const std::string& error_log) { link_to_child_returned_error = true; });

  {
    bool status_updated = false;
    child_view_watcher->GetStatus().Then([&](fidl::Result<ChildViewWatcher::GetStatus>& result) {
      if (result.is_ok()) {
        status_updated = true;
      }
    });
    EXPECT_FALSE(link_to_child_returned_error);
    EXPECT_FALSE(status_updated);

    child_view_watcher->GetStatus().Then([&](auto&) {});
    RunLoopUntilIdle();
    EXPECT_TRUE(link_to_child_returned_error);
    EXPECT_FALSE(status_updated);
  }

  {
    bool layout_updated = false;
    parent_viewport_watcher->GetLayout().Then(
        [&](fidl::Result<ParentViewportWatcher::GetLayout>& result) {
          if (result.is_ok()) {
            layout_updated = true;
          }
        });
    RunLoopUntilIdle();
    EXPECT_FALSE(link_to_parent_returned_error);
    EXPECT_TRUE(layout_updated);
  }

  {
    link_to_parent_returned_error = false;
    bool layout_updated = false;
    parent_viewport_watcher->GetLayout().Then(
        [&](fidl::Result<ParentViewportWatcher::GetLayout>& result) {
          if (result.is_ok()) {
            layout_updated = true;
          }
        });
    EXPECT_FALSE(link_to_parent_returned_error);
    EXPECT_FALSE(layout_updated);

    parent_viewport_watcher->GetLayout().Then([&](auto&) {});
    RunLoopUntilIdle();
    EXPECT_TRUE(link_to_parent_returned_error);
    EXPECT_FALSE(layout_updated);
  }
}

// LinkSystem::UpdateLinkWatchers() requires substantial setup to unit test:
// ParentViewportWatcher/ChildViewWatcher protocols attached to the correct TransformHandles in a
// correctly constructed global topology.  As a result, LinkSystem::UpdateLinkWatchers() is
// effectively tested in the Flatland unit tests in flatland_unittest.cc, since those tests simplify
// performing the correct setup.

}  // namespace test
}  // namespace flatland
