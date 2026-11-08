// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/power/all_drivers_element.h"

#include <fidl/fuchsia.power.broker/cpp/fidl.h>
#include <zircon/assert.h>

#include <unordered_set>

#include <gtest/gtest.h>

#include "src/devices/bin/driver_manager/power/power_manager.h"
#include "src/devices/bin/driver_manager/tests/driver_manager_test_base.h"

namespace driver_manager {

class FakeTopology : public fidl::Server<fuchsia_power_broker::Topology> {
 public:
  void Lease(LeaseRequest& request, LeaseCompleter::Sync& completer) override {
    lease_requests_.push_back(std::move(request));
    completer.Reply(fit::ok());
  }

  void AddElement(AddElementRequest& request, AddElementCompleter::Sync& completer) override {
    if (on_add_element_) {
      on_add_element_(request);
    }
    added_elements_.push_back(std::move(request));
    completer.Reply(fit::ok());
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_power_broker::Topology> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

  fit::function<void(AddElementRequest&)> on_add_element_;
  std::vector<LeaseRequest> lease_requests_;
  std::vector<AddElementRequest> added_elements_;
};

class FakeElementControl : public fidl::Server<fuchsia_power_broker::ElementControl> {
 public:
  void RegisterDependencyToken(RegisterDependencyTokenRequest& request,
                               RegisterDependencyTokenCompleter::Sync& completer) override {
    completer.Reply(fit::ok());
  }

  void UnregisterDependencyToken(UnregisterDependencyTokenRequest& request,
                                 UnregisterDependencyTokenCompleter::Sync& completer) override {
    completer.Reply(fit::ok());
  }

  void AddDependency(AddDependencyRequest& request,
                     AddDependencyCompleter::Sync& completer) override {
    // Like Power Broker, reject a dependency on an element that already has one. The required
    // element is identified by the koid of its power token.
    ZX_ASSERT(request.requires_token().has_value());
    if (!dependency_koids_.insert(GetKoid(*request.requires_token())).second) {
      already_exists_count_++;
      completer.Reply(fit::error(fuchsia_power_broker::ModifyDependencyError::kAlreadyExists));
      return;
    }
    added_dependencies_.push_back(std::move(request));
    if (auto_reply_add_dependency_) {
      completer.Reply(fit::ok());
    } else {
      add_dependency_completers_.push_back(completer.ToAsync());
    }
  }

  void RemoveDependency(RemoveDependencyRequest&,
                        RemoveDependencyCompleter::Sync& completer) override {
    completer.Reply(fit::ok());
  }

  // Simulates Power Broker removing the dependency on the element that |token| belongs to. Power
  // Broker does this when the element is removed (e.g. when its node restarts), because the
  // dependency is added with |remove_with_required_element|.
  void RemoveDependencyOn(const zx::event& token) { dependency_koids_.erase(GetKoid(token)); }

  void ReplyAddDependencies() {
    for (auto& comp : add_dependency_completers_) {
      comp.Reply(fit::ok());
    }
    add_dependency_completers_.clear();
  }

  void OpenStatusChannel(OpenStatusChannelRequest& request,
                         OpenStatusChannelCompleter::Sync& completer) override {}

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_power_broker::ElementControl> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {}

  bool auto_reply_add_dependency_ = true;
  std::vector<AddDependencyCompleter::Async> add_dependency_completers_;
  // The dependencies that were accepted, in the order they were added.
  std::vector<fuchsia_power_broker::LevelDependency> added_dependencies_;
  // The number of AddDependency requests rejected because the dependency already existed.
  size_t already_exists_count_ = 0;

  static zx_koid_t GetKoid(const zx::event& event) {
    zx_info_handle_basic_t info;
    ZX_ASSERT(event.get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr) == ZX_OK);
    return info.koid;
  }

 private:
  // The koids of the power tokens of the elements that currently have an accepted dependency.
  std::unordered_set<zx_koid_t> dependency_koids_;
};

class AllDriversTestNodeManager : public TestNodeManagerBase {
 public:
  void OnNodeUnbinding(std::shared_ptr<const Node> node) override {
    if (element_) {
      element_->OnNodeUnbinding(std::move(node));
    }
  }

  AllDriversElement* element_ = nullptr;
};

class AllDriversElementTest : public DriverManagerTestBase {
 public:
  NodeManager* GetNodeManager() override { return &node_manager_; }

  void SetElementForNodeManager(AllDriversElement* element) { node_manager_.element_ = element; }

  static void CompleteStartupTransition(AllDriversElement& element) {
    element.CompleteStartupTransition();
  }

  static void SetCurrentLevel(AllDriversElement& element, uint32_t level) {
    element.current_level_ = level;
  }

  static void SetTransitionComplete(AllDriversElement& element, bool complete) {
    element.transition_complete_ = complete;
  }

 private:
  AllDriversTestNodeManager node_manager_;
};

}  // namespace driver_manager

namespace driver_manager {
namespace {

TEST_F(AllDriversElementTest, TestSetLevel) {
  AllDriversElement element(nullptr, root());
  auto [runner_client, runner_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  auto [lessor_client, lessor_server] = fidl::Endpoints<fuchsia_power_broker::Lessor>::Create();

  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
      .element_runner = std::move(runner_server),
      .lessor = fidl::Client<fuchsia_power_broker::Lessor>(std::move(lessor_client), dispatcher()),
  };

  element.AttachElement(dispatcher(), std::move(handles));
  SetTransitionComplete(element, true);
  SetCurrentLevel(element, 1);

  fidl::WireClient<fuchsia_power_broker::ElementRunner> client(std::move(runner_client),
                                                               dispatcher());

  // Create a bound leaf node and set its startup lease.
  auto child = CreateNode("child_node");
  child->UpdateBoundStateForTesting(true);
  zx::eventpair startup_lease, lease_peer;
  ASSERT_EQ(zx::eventpair::create(0, &startup_lease, &lease_peer), ZX_OK);
  child->set_startup_lease_for_testing(std::move(startup_lease));

  element.OnNodeBound(child);
  RunLoopUntilIdle();

  // SetLevel(1)
  {
    bool called = false;
    client->SetLevel(1).Then(
        [&called](fidl::WireUnownedResult<fuchsia_power_broker::ElementRunner::SetLevel>& result) {
          EXPECT_TRUE(result.ok());
          called = true;
        });

    RunLoopUntilIdle();
    EXPECT_TRUE(called);
  }

  // SetLevel(0)
  {
    bool called = false;
    client->SetLevel(0).Then(
        [&called](fidl::WireUnownedResult<fuchsia_power_broker::ElementRunner::SetLevel>& result) {
          EXPECT_TRUE(result.ok());
          called = true;
        });

    RunLoopUntilIdle();
    EXPECT_TRUE(called);
  }
}

// Test CompleteStartupTransition on a Linear Chain topology: root -> parent -> child.
// After transition, only the child (the leaf) should add a dependency, and both startup
// leases should be released/dropped.
TEST_F(AllDriversElementTest, TestCompleteStartupTransitionLinearChain) {
  auto parent = CreateNode("parent", root());
  auto child = CreateNode("child", parent);

  parent->UpdateBoundStateForTesting(true);
  child->UpdateBoundStateForTesting(true);

  zx::eventpair lease_a, lease_a_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease_a, &lease_a_peer), ZX_OK);
  parent->set_startup_lease_for_testing(std::move(lease_a));

  zx::eventpair lease_b, lease_b_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease_b, &lease_b_peer), ZX_OK);
  child->set_startup_lease_for_testing(std::move(lease_b));

  AllDriversElement element(nullptr, root());
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetCurrentLevel(element, 1);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  // Only child should add a dependency.
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);

  // Both startup leases should be taken/released from nodes.
  auto lease_a_taken = parent->TakeStartupLease();
  EXPECT_TRUE(!lease_a_taken.has_value() || !lease_a_taken->is_valid());

  auto lease_b_taken = child->TakeStartupLease();
  EXPECT_TRUE(!lease_b_taken.has_value() || !lease_b_taken->is_valid());

  zx_signals_t pending = 0;
  EXPECT_EQ(lease_a_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);
  EXPECT_EQ(lease_b_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);
}

// Test CompleteStartupTransition with a Branching Tree topology: root -> child_a, root -> child_b.
// After transition, both child_a and child_b should add dependencies and have leases dropped.
TEST_F(AllDriversElementTest, TestCompleteStartupTransitionBranchingTree) {
  auto child_a = CreateNode("child_a", root());
  auto child_b = CreateNode("child_b", root());

  child_a->UpdateBoundStateForTesting(true);
  child_b->UpdateBoundStateForTesting(true);

  zx::eventpair lease_a, lease_a_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease_a, &lease_a_peer), ZX_OK);
  child_a->set_startup_lease_for_testing(std::move(lease_a));

  zx::eventpair lease_b, lease_b_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease_b, &lease_b_peer), ZX_OK);
  child_b->set_startup_lease_for_testing(std::move(lease_b));

  AllDriversElement element(nullptr, root());
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetCurrentLevel(element, 1);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  EXPECT_EQ(fake_control.added_dependencies_.size(), 2u);

  auto lease_a_taken = child_a->TakeStartupLease();
  EXPECT_TRUE(!lease_a_taken.has_value() || !lease_a_taken->is_valid());

  auto lease_b_taken = child_b->TakeStartupLease();
  EXPECT_TRUE(!lease_b_taken.has_value() || !lease_b_taken->is_valid());

  zx_signals_t pending = 0;
  EXPECT_EQ(lease_a_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);
  EXPECT_EQ(lease_b_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);
}

// Test CompleteStartupTransition with a Diamond DAG topology:
// root -> parent_a -> composite, root -> parent_b -> composite.
// After transition, only the composite node should add a dependency and leases dropped.
TEST_F(AllDriversElementTest, TestCompleteStartupTransitionDiamondDAG) {
  auto parent_a = CreateNode("parent_a", root());
  auto parent_b = CreateNode("parent_b", root());

  std::vector<std::weak_ptr<Node>> parents = {parent_a, parent_b};
  auto composite = CreateCompositeNode("composite", parents, {{}, {}});

  parent_a->UpdateBoundStateForTesting(true);
  parent_b->UpdateBoundStateForTesting(true);
  composite->UpdateBoundStateForTesting(true);

  zx::eventpair lease_a, lease_a_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease_a, &lease_a_peer), ZX_OK);
  parent_a->set_startup_lease_for_testing(std::move(lease_a));

  zx::eventpair lease_b, lease_b_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease_b, &lease_b_peer), ZX_OK);
  parent_b->set_startup_lease_for_testing(std::move(lease_b));

  zx::eventpair lease_c, lease_c_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease_c, &lease_c_peer), ZX_OK);
  composite->set_startup_lease_for_testing(std::move(lease_c));

  AllDriversElement element(nullptr, root());
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetCurrentLevel(element, 1);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  EXPECT_EQ(fake_control.added_dependencies_.size(), 1u);

  auto lease_a_taken = parent_a->TakeStartupLease();
  EXPECT_TRUE(!lease_a_taken.has_value() || !lease_a_taken->is_valid());

  auto lease_b_taken = parent_b->TakeStartupLease();
  EXPECT_TRUE(!lease_b_taken.has_value() || !lease_b_taken->is_valid());

  auto lease_c_taken = composite->TakeStartupLease();
  EXPECT_TRUE(!lease_c_taken.has_value() || !lease_c_taken->is_valid());

  zx_signals_t pending = 0;
  EXPECT_EQ(lease_a_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);
  EXPECT_EQ(lease_b_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);
  EXPECT_EQ(lease_c_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);
}

TEST_F(AllDriversElementTest, TestOnNodeBoundLeaf) {
  AllDriversElement element(nullptr, root());
  auto [runner_client, runner_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  auto [lessor_client, lessor_server] = fidl::Endpoints<fuchsia_power_broker::Lessor>::Create();

  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
      .element_runner = std::move(runner_server),
      .lessor = fidl::Client<fuchsia_power_broker::Lessor>(std::move(lessor_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetTransitionComplete(element, true);

  // Create a bound leaf node
  auto child = CreateNode("child_node");
  child->UpdateBoundStateForTesting(true);

  element.OnNodeBound(child);

  RunLoopUntilIdle();

  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_EQ(fake_control.added_dependencies_[0].dependent_level(), 1u);
  EXPECT_TRUE(fake_control.added_dependencies_[0].remove_with_required_element().value_or(false));

  zx::event expected_token = child->DuplicatePowerToken();
  ASSERT_TRUE(expected_token.is_valid());

  // Verify requires_token is matching by comparing koids
  zx_info_handle_basic_t actual_info, expected_info;
  ASSERT_TRUE(fake_control.added_dependencies_[0].requires_token().has_value());
  ASSERT_EQ(fake_control.added_dependencies_[0].requires_token()->get_info(
                ZX_INFO_HANDLE_BASIC, &actual_info, sizeof(actual_info), nullptr, nullptr),
            ZX_OK);
  ASSERT_EQ(expected_token.get_info(ZX_INFO_HANDLE_BASIC, &expected_info, sizeof(expected_info),
                                    nullptr, nullptr),
            ZX_OK);
  EXPECT_EQ(actual_info.koid, expected_info.koid);
}

TEST_F(AllDriversElementTest, TestOnNodeBoundNonLeaf) {
  AllDriversElement element(nullptr, root());
  auto [runner_client, runner_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  auto [lessor_client, lessor_server] = fidl::Endpoints<fuchsia_power_broker::Lessor>::Create();

  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
      .element_runner = std::move(runner_server),
      .lessor = fidl::Client<fuchsia_power_broker::Lessor>(std::move(lessor_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetTransitionComplete(element, true);

  // Create a parent node (non-leaf because it will have a bound child)
  auto parent = CreateNode("parent_node");
  parent->UpdateBoundStateForTesting(true);

  auto child = CreateNode("child_node", parent);
  child->UpdateBoundStateForTesting(true);

  element.OnNodeBound(parent);

  RunLoopUntilIdle();

  // Since parent has a bound descendant (child), it is not a leaf and shouldn't add a dependency.
  EXPECT_TRUE(fake_control.added_dependencies_.empty());
}

TEST_F(AllDriversElementTest, TestCompleteStartupTransitionAddsDependencies) {
  auto parent = CreateNode("parent", root());
  auto child = CreateNode("child", parent);

  parent->UpdateBoundStateForTesting(true);
  child->UpdateBoundStateForTesting(true);

  AllDriversElement element(nullptr, root());
  auto [runner_client, runner_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  auto [lessor_client, lessor_server] = fidl::Endpoints<fuchsia_power_broker::Lessor>::Create();

  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
      .element_runner = std::move(runner_server),
      .lessor = fidl::Client<fuchsia_power_broker::Lessor>(std::move(lessor_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetCurrentLevel(element, 1);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  // Only leaf node (child) should have added a dependency
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_EQ(fake_control.added_dependencies_[0].dependent_level(), 1u);

  // A node that was already bound when CompleteStartupTransition() ran can still have
  // OnNodeBound() called for it afterwards. The second request is rejected with ALREADY_EXISTS,
  // which is ignored.
  element.OnNodeBound(child);
  RunLoopUntilIdle();
  EXPECT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_EQ(fake_control.already_exists_count_, 1u);
}

TEST_F(AllDriversElementTest, TestOnNodeBoundAfterRestartAddsDependencyAgain) {
  AllDriversElement element(nullptr, root());
  auto [runner_client, runner_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  auto [lessor_client, lessor_server] = fidl::Endpoints<fuchsia_power_broker::Lessor>::Create();

  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
      .element_runner = std::move(runner_server),
      .lessor = fidl::Client<fuchsia_power_broker::Lessor>(std::move(lessor_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetTransitionComplete(element, true);

  auto child = CreateNode("child_node");
  child->UpdateBoundStateForTesting(true);
  zx::eventpair first_lease, first_lease_peer;
  ASSERT_EQ(zx::eventpair::create(0, &first_lease, &first_lease_peer), ZX_OK);
  child->set_startup_lease_for_testing(std::move(first_lease));

  element.OnNodeBound(child);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);

  // Simulate the node restarting. Its old power element is removed, and Power Broker removes the
  // dependency on it along with the element. The node keeps its topological path and power token,
  // but gets a new power element and a new startup lease.
  fake_control.RemoveDependencyOn(child->DuplicatePowerToken());
  zx::eventpair second_lease, second_lease_peer;
  ASSERT_EQ(zx::eventpair::create(0, &second_lease, &second_lease_peer), ZX_OK);
  child->set_startup_lease_for_testing(std::move(second_lease));

  element.OnNodeBound(child);
  RunLoopUntilIdle();

  // The restarted node gets its own dependency, which Power Broker accepts.
  EXPECT_EQ(fake_control.added_dependencies_.size(), 2u);
  EXPECT_EQ(fake_control.already_exists_count_, 0u);

  // The new startup lease is dropped once the dependency has been added.
  zx_signals_t pending = 0;
  EXPECT_EQ(
      second_lease_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
      ZX_OK);
}

TEST_F(AllDriversElementTest, TestLeaseAllDrivers) {
  auto [topology_client, topology_server] =
      fidl::Endpoints<fuchsia_power_broker::Topology>::Create();

  FakeTopology fake_topology;
  fidl::ServerBindingGroup<fuchsia_power_broker::Topology> bindings;
  bindings.AddBinding(dispatcher(), std::move(topology_server), &fake_topology,
                      fidl::kIgnoreBindingClosure);

  PowerManager power_manager(dispatcher(), std::move(topology_client), std::nullopt, false, root());

  // Setup a topology: parent_node (unbound) -> child_a (bound), child_b (unbound) -> grandchild
  // (bound)
  auto parent = CreateNode("parent_node");
  auto child_a = CreateNode("child_a", parent);
  child_a->UpdateBoundStateForTesting(true);

  auto child_b = CreateNode("child_b", parent);
  child_b->UpdateBoundStateForTesting(false);

  auto grandchild = CreateNode("grandchild", child_b);
  grandchild->UpdateBoundStateForTesting(true);

  // Call LeaseAllDrivers
  power_manager.LeaseAllDrivers(parent, [] {});

  RunLoopUntilIdle();

  // It should have acquired leases for child_a and grandchild (both are bound nodes in the
  // topology)
  ASSERT_EQ(fake_topology.lease_requests_.size(), 2u);

  // Collect lease names to verify
  std::unordered_set<std::string> leased_names;
  for (const auto& req : fake_topology.lease_requests_) {
    ASSERT_TRUE(req.lease_name().has_value());
    leased_names.insert(*req.lease_name());
  }

  EXPECT_TRUE(leased_names.contains("child_a"));
  EXPECT_TRUE(leased_names.contains("grandchild"));
  EXPECT_FALSE(leased_names.contains("parent_node"));
  EXPECT_FALSE(leased_names.contains("child_b"));
}

TEST_F(AllDriversElementTest, TestStartupLeaseHeldUntilAddDependencyCompletes) {
  AllDriversElement element(nullptr, root());
  auto [runner_client, runner_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  auto [lessor_client, lessor_server] = fidl::Endpoints<fuchsia_power_broker::Lessor>::Create();

  FakeElementControl fake_control;
  fake_control.auto_reply_add_dependency_ = false;

  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
      .element_runner = std::move(runner_server),
      .lessor = fidl::Client<fuchsia_power_broker::Lessor>(std::move(lessor_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetTransitionComplete(element, true);

  auto child = CreateNode("child_node");
  child->UpdateBoundStateForTesting(true);
  zx::eventpair startup_lease, lease_peer;
  ASSERT_EQ(zx::eventpair::create(0, &startup_lease, &lease_peer), ZX_OK);
  child->set_startup_lease_for_testing(std::move(startup_lease));

  element.OnNodeBound(child);
  RunLoopUntilIdle();

  // AddDependency request has been received, but not yet replied to.
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);

  // The startup lease should still be held by the in-flight AddDependency request.
  zx_signals_t pending = 0;
  EXPECT_EQ(lease_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_ERR_TIMED_OUT);

  // Complete the AddDependency call.
  fake_control.ReplyAddDependencies();
  RunLoopUntilIdle();

  // The startup lease should now be dropped.
  EXPECT_EQ(lease_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);
}

TEST_F(AllDriversElementTest, TestCompleteStartupTransitionDeepestPowerEnabledNodes) {
  // Topology:
  // root -> branch1_pe (PE) -> branch1_leaf_non_pe (bound, no PE)
  // root -> branch2_non_pe (bound, no PE) -> branch2_leaf_pe (PE)
  auto branch1_pe = CreateNode("branch1_pe", root());
  auto branch1_leaf_non_pe = CreateNode("branch1_leaf_non_pe", branch1_pe);
  branch1_pe->UpdateBoundStateForTesting(true, true);
  branch1_leaf_non_pe->UpdateBoundStateForTesting(true, false);

  auto branch2_non_pe = CreateNode("branch2_non_pe", root());
  auto branch2_leaf_pe = CreateNode("branch2_leaf_pe", branch2_non_pe);
  branch2_non_pe->UpdateBoundStateForTesting(true, false);
  branch2_leaf_pe->UpdateBoundStateForTesting(true, true);

  AllDriversElement element(nullptr, root());
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetCurrentLevel(element, 1);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  // Both branch1_pe (whose only child has no PE) and branch2_leaf_pe should be added.
  ASSERT_EQ(fake_control.added_dependencies_.size(), 2u);

  zx_info_handle_basic_t b1_info, b2_info;
  ASSERT_EQ(branch1_pe->DuplicatePowerToken().get_info(ZX_INFO_HANDLE_BASIC, &b1_info,
                                                       sizeof(b1_info), nullptr, nullptr),
            ZX_OK);
  ASSERT_EQ(branch2_leaf_pe->DuplicatePowerToken().get_info(ZX_INFO_HANDLE_BASIC, &b2_info,
                                                            sizeof(b2_info), nullptr, nullptr),
            ZX_OK);

  std::unordered_set<zx_koid_t> added_koids;
  for (const auto& dep : fake_control.added_dependencies_) {
    zx_info_handle_basic_t info;
    ASSERT_EQ(
        dep.requires_token()->get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr),
        ZX_OK);
    added_koids.insert(info.koid);
  }
  EXPECT_TRUE(added_koids.contains(b1_info.koid));
  EXPECT_TRUE(added_koids.contains(b2_info.koid));
}

TEST_F(AllDriversElementTest, TestOnNodeBoundWithNonPowerEnabledNodes) {
  AllDriversElement element(nullptr, root());
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetTransitionComplete(element, true);

  // 1. Node without PE -> OnNodeBound does not add dependency.
  auto non_pe = CreateNode("non_pe", root());
  non_pe->UpdateBoundStateForTesting(true, false);
  element.OnNodeBound(non_pe);
  RunLoopUntilIdle();
  EXPECT_TRUE(fake_control.added_dependencies_.empty());

  // 2. Node with PE whose child is bound without PE -> OnNodeBound adds dependency.
  auto pe_parent = CreateNode("pe_parent", root());
  pe_parent->UpdateBoundStateForTesting(true, true);
  auto non_pe_child = CreateNode("non_pe_child", pe_parent);
  non_pe_child->UpdateBoundStateForTesting(true, false);

  element.OnNodeBound(pe_parent);
  RunLoopUntilIdle();
  EXPECT_EQ(fake_control.added_dependencies_.size(), 1u);
}

bool IsPeerClosed(const zx::eventpair& handle) {
  zx_signals_t pending = 0;
  return handle.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending) == ZX_OK;
}

TEST_F(AllDriversElementTest, TestZeroPowerEnabledDrivers) {
  auto child1 = CreateNode("child1", root());
  auto child2 = CreateNode("child2", child1);
  child1->UpdateBoundStateForTesting(true, false);
  child2->UpdateBoundStateForTesting(true, false);

  zx::eventpair bootup_lease, bootup_lease_peer;
  ASSERT_EQ(zx::eventpair::create(0, &bootup_lease, &bootup_lease_peer), ZX_OK);

  AllDriversElement element(nullptr, root(), std::move(bootup_lease));
  auto [runner_client, runner_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
      .element_runner = std::move(runner_server),
  };
  element.AttachElement(dispatcher(), std::move(handles));

  // OnBootupComplete before SetLevel(1) keeps bootup_lease until SetLevel(1) arrives.
  element.OnBootupComplete();
  RunLoopUntilIdle();
  EXPECT_FALSE(IsPeerClosed(bootup_lease_peer));

  fidl::WireClient<fuchsia_power_broker::ElementRunner> runner(std::move(runner_client),
                                                               dispatcher());
  runner->SetLevel(1).Then(
      [](fidl::WireUnownedResult<fuchsia_power_broker::ElementRunner::SetLevel>& result) {
        EXPECT_TRUE(result.ok());
      });
  RunLoopUntilIdle();
  EXPECT_TRUE(fake_control.added_dependencies_.empty());
  EXPECT_TRUE(IsPeerClosed(bootup_lease_peer));
}

TEST_F(AllDriversElementTest, TestLeaseAllDriversMixedPowerTopology) {
  auto [topology_client, topology_server] =
      fidl::Endpoints<fuchsia_power_broker::Topology>::Create();

  FakeTopology fake_topology;
  fidl::ServerBindingGroup<fuchsia_power_broker::Topology> bindings;
  bindings.AddBinding(dispatcher(), std::move(topology_server), &fake_topology,
                      fidl::kIgnoreBindingClosure);

  PowerManager power_manager(dispatcher(), std::move(topology_client), std::nullopt, false, root());

  auto parent = CreateNode("parent_node");
  parent->UpdateBoundStateForTesting(true, false);  // bound, no PE

  auto child_pe = CreateNode("child_pe", parent);
  child_pe->UpdateBoundStateForTesting(true, true);  // bound, has PE

  auto child_no_pe = CreateNode("child_no_pe", parent);
  child_no_pe->UpdateBoundStateForTesting(true, false);  // bound, no PE

  auto grandchild_pe = CreateNode("grandchild_pe", child_no_pe);
  grandchild_pe->UpdateBoundStateForTesting(true, true);  // bound, has PE

  bool completed = false;
  power_manager.LeaseAllDrivers(parent, [&completed] { completed = true; });
  RunLoopUntilIdle();
  EXPECT_TRUE(completed);

  // Only child_pe and grandchild_pe should be leased.
  ASSERT_EQ(fake_topology.lease_requests_.size(), 2u);
  std::unordered_set<std::string> leased_names;
  for (const auto& req : fake_topology.lease_requests_) {
    ASSERT_TRUE(req.lease_name().has_value());
    leased_names.insert(*req.lease_name());
  }
  EXPECT_TRUE(leased_names.contains("child_pe"));
  EXPECT_TRUE(leased_names.contains("grandchild_pe"));
  EXPECT_FALSE(leased_names.contains("parent_node"));
  EXPECT_FALSE(leased_names.contains("child_no_pe"));
}

class FakeCpuElementManager : public fidl::Server<fuchsia_power_system::CpuElementManager> {
 public:
  // NOLINTNEXTLINE(readability-convert-member-functions-to-static)
  void GetCpuDependencyToken(GetCpuDependencyTokenCompleter::Sync& completer) override {
    zx::event token;
    ZX_ASSERT(zx::event::create(0, &token) == ZX_OK);
    fuchsia_power_system::Cpu res;
    res.assertive_dependency_token(std::move(token));
    completer.Reply(std::move(res));
  }

  // NOLINTNEXTLINE(readability-convert-member-functions-to-static)
  void AddExecutionStateDependency(AddExecutionStateDependencyRequest& request,
                                   AddExecutionStateDependencyCompleter::Sync& completer) override {
    execution_state_deps_.push_back(std::move(request));
    completer.Reply(fit::ok());
  }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_power_system::CpuElementManager> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {}

  std::vector<AddExecutionStateDependencyRequest> execution_state_deps_;
};

// Drives a PowerManager with a fake Power Broker topology and a fake SAG CpuElementManager, with
// |wait_for_storage_token| enabled so AllDrivers creation depends on both the CPU and storage
// tokens.
class PowerManagerBootupTest : public AllDriversElementTest {
 public:
  void SetUp() override {
    AllDriversElementTest::SetUp();
    auto [topology_client, topology_server] =
        fidl::Endpoints<fuchsia_power_broker::Topology>::Create();
    topology_bindings_.AddBinding(dispatcher(), std::move(topology_server), &fake_topology_,
                                  fidl::kIgnoreBindingClosure);
    fake_topology_.on_add_element_ = [this](fuchsia_power_broker::ElementSchema& req) {
      control_bindings_.AddBinding(
          dispatcher(), std::move(*req.element_control()),
          req.element_name() == "AllDrivers" ? &all_drivers_control_ : &storage_control_,
          fidl::kIgnoreBindingClosure);
    };

    auto [cpu_mgr_client, cpu_mgr_server] =
        fidl::Endpoints<fuchsia_power_system::CpuElementManager>::Create();
    sag_bindings_.AddBinding(dispatcher(), std::move(cpu_mgr_server), &fake_sag_,
                             fidl::kIgnoreBindingClosure);

    power_manager_.emplace(dispatcher(), std::move(topology_client), std::move(cpu_mgr_client),
                           /*wait_for_storage_token=*/true, root());

    auto [dm_cpu_client, dm_cpu_server] =
        fidl::Endpoints<fuchsia_power_system::CpuElementManager>::Create();
    power_manager_bindings_.AddBinding(dispatcher(), std::move(dm_cpu_server), &*power_manager_,
                                       fidl::kIgnoreBindingClosure);
    storage_driver_client_.Bind(std::move(dm_cpu_client), dispatcher());
  }

  // Simulates the storage driver handing its token to driver_manager.
  void SendStorageToken() {
    zx::event storage_token;
    ASSERT_EQ(zx::event::create(0, &storage_token), ZX_OK);
    fuchsia_power_system::CpuElementManagerAddExecutionStateDependencyRequest req;
    req.dependency_token(std::move(storage_token));
    req.power_level(1);
    storage_driver_client_->AddExecutionStateDependency(std::move(req))
        .Then([](fidl::Result<fuchsia_power_system::CpuElementManager::AddExecutionStateDependency>&
                     res) { EXPECT_TRUE(res.is_ok()); });
    RunLoopUntilIdle();
  }

  // Verifies the AllDrivers schema (level 1, CPU + DF-Storage deps, initial lease) and takes the
  // bootup lease peer and ElementRunner client out of it.
  void TakeAllDriversHandles(zx::eventpair* bootup_lease_peer,
                             fidl::WireClient<fuchsia_power_broker::ElementRunner>* runner) {
    ASSERT_EQ(fake_topology_.added_elements_.size(), 2u);
    auto& schema = fake_topology_.added_elements_.back();
    EXPECT_EQ(schema.element_name(), "AllDrivers");
    EXPECT_EQ(schema.initial_current_level(), 1u);
    ASSERT_TRUE(schema.dependencies().has_value());
    EXPECT_EQ(schema.dependencies()->size(), 2u);
    ASSERT_TRUE(schema.initial_lease_token().has_value());
    *bootup_lease_peer = std::move(*schema.initial_lease_token());
    runner->Bind(std::move(*schema.element_runner()), dispatcher());
    ASSERT_EQ(fake_sag_.execution_state_deps_.size(), 1u);
  }

  static void SetLevel1(fidl::WireClient<fuchsia_power_broker::ElementRunner>& runner) {
    runner->SetLevel(1).Then(
        [](fidl::WireUnownedResult<fuchsia_power_broker::ElementRunner::SetLevel>& result) {
          EXPECT_TRUE(result.ok());
        });
  }

 protected:
  FakeTopology fake_topology_;
  FakeCpuElementManager fake_sag_;
  FakeElementControl storage_control_;
  FakeElementControl all_drivers_control_;
  fidl::ServerBindingGroup<fuchsia_power_broker::Topology> topology_bindings_;
  fidl::ServerBindingGroup<fuchsia_power_system::CpuElementManager> sag_bindings_;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> control_bindings_;
  std::optional<PowerManager> power_manager_;
  // Declared after |power_manager_| so it is destroyed first.
  fidl::ServerBindingGroup<fuchsia_power_system::CpuElementManager> power_manager_bindings_;
  fidl::Client<fuchsia_power_system::CpuElementManager> storage_driver_client_;
};

TEST_F(PowerManagerBootupTest, TestEarlyAllDriversCreationBeforeBootup) {
  power_manager_->FetchCpuToken();
  RunLoopUntilIdle();
  // The CPU token alone is not enough while waiting for the storage token.
  EXPECT_TRUE(fake_topology_.added_elements_.empty());

  // Before OnBootupComplete(), AllDrivers is created at level 1 with a bootup lease and SAG gets
  // its execution state dependency.
  SendStorageToken();
  zx::eventpair bootup_lease_peer;
  fidl::WireClient<fuchsia_power_broker::ElementRunner> runner;
  ASSERT_NO_FATAL_FAILURE(TakeAllDriversHandles(&bootup_lease_peer, &runner));
  all_drivers_control_.auto_reply_add_dependency_ = false;
  SetLevel1(runner);

  // Nodes binding before OnBootupComplete() must not trigger transition or drop bootup_lease.
  auto parent = CreateNode("parent", root());
  parent->UpdateBoundStateForTesting(true);
  auto child = CreateNode("child", parent);
  child->UpdateBoundStateForTesting(true);
  zx::eventpair child_lease, child_lease_peer;
  ASSERT_EQ(zx::eventpair::create(0, &child_lease, &child_lease_peer), ZX_OK);
  child->set_startup_lease_for_testing(std::move(child_lease));
  power_manager_->OnNodeBound(parent);
  power_manager_->OnNodeBound(child);
  RunLoopUntilIdle();
  EXPECT_TRUE(all_drivers_control_.added_dependencies_.empty());
  EXPECT_FALSE(IsPeerClosed(bootup_lease_peer));

  // OnBootupComplete() adds only leaf child and holds both leases until AddDependency replies.
  power_manager_->OnBootupComplete();
  RunLoopUntilIdle();
  ASSERT_EQ(all_drivers_control_.added_dependencies_.size(), 1u);
  EXPECT_FALSE(IsPeerClosed(child_lease_peer));
  EXPECT_FALSE(IsPeerClosed(bootup_lease_peer));

  all_drivers_control_.ReplyAddDependencies();
  RunLoopUntilIdle();
  EXPECT_TRUE(IsPeerClosed(child_lease_peer));
  EXPECT_TRUE(IsPeerClosed(bootup_lease_peer));
}

TEST_F(PowerManagerBootupTest, TestTokensArriveAfterBootupComplete) {
  auto child = CreateNode("child", root());
  child->UpdateBoundStateForTesting(true);
  zx::eventpair child_lease, child_lease_peer;
  ASSERT_EQ(zx::eventpair::create(0, &child_lease, &child_lease_peer), ZX_OK);
  child->set_startup_lease_for_testing(std::move(child_lease));

  // Bootup completes before either token is available: nothing can be created yet.
  power_manager_->OnBootupComplete();
  power_manager_->FetchCpuToken();
  RunLoopUntilIdle();
  EXPECT_TRUE(fake_topology_.added_elements_.empty());

  // The storage token arrives last and creates AllDrivers, which already knows bootup is done.
  SendStorageToken();
  zx::eventpair bootup_lease_peer;
  fidl::WireClient<fuchsia_power_broker::ElementRunner> runner;
  ASSERT_NO_FATAL_FAILURE(TakeAllDriversHandles(&bootup_lease_peer, &runner));
  all_drivers_control_.auto_reply_add_dependency_ = false;
  RunLoopUntilIdle();
  EXPECT_TRUE(all_drivers_control_.added_dependencies_.empty());
  EXPECT_FALSE(IsPeerClosed(bootup_lease_peer));

  // SetLevel(1) runs the startup transition; leases are held until AddDependency replies.
  SetLevel1(runner);
  RunLoopUntilIdle();
  ASSERT_EQ(all_drivers_control_.added_dependencies_.size(), 1u);
  EXPECT_FALSE(IsPeerClosed(child_lease_peer));
  EXPECT_FALSE(IsPeerClosed(bootup_lease_peer));

  all_drivers_control_.ReplyAddDependencies();
  RunLoopUntilIdle();
  EXPECT_TRUE(IsPeerClosed(child_lease_peer));
  EXPECT_TRUE(IsPeerClosed(bootup_lease_peer));
}

TEST_F(AllDriversElementTest, TestOnNodeUnbindingPromotesParentToLeaf) {
  auto parent = CreateNode("parent", root());
  auto child = CreateNode("child", parent);

  parent->UpdateBoundStateForTesting(true);
  child->UpdateBoundStateForTesting(true);

  AllDriversElement element(nullptr, root());
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetCurrentLevel(element, 1);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_EQ(FakeElementControl::GetKoid(*fake_control.added_dependencies_[0].requires_token()),
            FakeElementControl::GetKoid(child->DuplicatePowerToken()));

  // Unbind the leaf child node; parent should now be promoted to a leaf node.
  child->UpdateBoundStateForTesting(false);
  element.OnNodeUnbinding(child);
  RunLoopUntilIdle();

  ASSERT_EQ(fake_control.added_dependencies_.size(), 2u);
  EXPECT_EQ(FakeElementControl::GetKoid(*fake_control.added_dependencies_[1].requires_token()),
            FakeElementControl::GetKoid(parent->DuplicatePowerToken()));
}

TEST_F(AllDriversElementTest, TestOnNodeUnbindingParentWithRemainingBoundChild) {
  auto parent = CreateNode("parent", root());
  auto child_a = CreateNode("child_a", parent);
  auto child_b = CreateNode("child_b", parent);

  parent->UpdateBoundStateForTesting(true);
  child_a->UpdateBoundStateForTesting(true);
  child_b->UpdateBoundStateForTesting(true);

  AllDriversElement element(nullptr, root());
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetCurrentLevel(element, 1);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  ASSERT_EQ(fake_control.added_dependencies_.size(), 2u);

  // Unbinding child_a leaves child_b bound, so parent is not yet a leaf.
  child_a->UpdateBoundStateForTesting(false);
  element.OnNodeUnbinding(child_a);
  RunLoopUntilIdle();
  EXPECT_EQ(fake_control.added_dependencies_.size(), 2u);

  // Unbinding child_b makes parent a leaf.
  child_b->UpdateBoundStateForTesting(false);
  element.OnNodeUnbinding(child_b);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 3u);
  EXPECT_EQ(FakeElementControl::GetKoid(*fake_control.added_dependencies_[2].requires_token()),
            FakeElementControl::GetKoid(parent->DuplicatePowerToken()));
}

TEST_F(AllDriversElementTest, TestOnNodeUnbindingTraversesThroughUnboundIntermediateNodes) {
  // Mirrors topology: usb-phy (bound) -> dwc3-phy (unbound) -> dwc3 (bound).
  auto usb_phy = CreateNode("usb-phy", root());
  auto dwc3_phy = CreateNode("dwc3-phy", usb_phy);
  auto dwc3 = CreateNode("dwc3", dwc3_phy);

  usb_phy->UpdateBoundStateForTesting(true);
  dwc3_phy->UpdateBoundStateForTesting(false);
  dwc3->UpdateBoundStateForTesting(true);

  AllDriversElement element(nullptr, root());
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetCurrentLevel(element, 1);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_EQ(FakeElementControl::GetKoid(*fake_control.added_dependencies_[0].requires_token()),
            FakeElementControl::GetKoid(dwc3->DuplicatePowerToken()));

  // Unbind dwc3; AllDriversElement should traverse through unbound dwc3-phy to usb-phy.
  dwc3->UpdateBoundStateForTesting(false);
  element.OnNodeUnbinding(dwc3);
  RunLoopUntilIdle();

  ASSERT_EQ(fake_control.added_dependencies_.size(), 2u);
  EXPECT_EQ(FakeElementControl::GetKoid(*fake_control.added_dependencies_[1].requires_token()),
            FakeElementControl::GetKoid(usb_phy->DuplicatePowerToken()));
}

TEST_F(AllDriversElementTest, TestOnNodeUnbindingDiamondDAG) {
  auto parent_a = CreateNode("parent_a", root());
  auto parent_b = CreateNode("parent_b", root());

  std::vector<std::weak_ptr<Node>> parents = {parent_a, parent_b};
  auto composite = CreateCompositeNode("composite", parents, {{}, {}});

  parent_a->UpdateBoundStateForTesting(true);
  parent_b->UpdateBoundStateForTesting(true);
  composite->UpdateBoundStateForTesting(true);

  AllDriversElement element(nullptr, root());
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetCurrentLevel(element, 1);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_EQ(FakeElementControl::GetKoid(*fake_control.added_dependencies_[0].requires_token()),
            FakeElementControl::GetKoid(composite->DuplicatePowerToken()));

  // Unbind composite; both parent_a and parent_b should become leaves and get dependencies.
  composite->UpdateBoundStateForTesting(false);
  element.OnNodeUnbinding(composite);
  RunLoopUntilIdle();

  ASSERT_EQ(fake_control.added_dependencies_.size(), 3u);
  std::unordered_set<zx_koid_t> parent_koids = {
      FakeElementControl::GetKoid(*fake_control.added_dependencies_[1].requires_token()),
      FakeElementControl::GetKoid(*fake_control.added_dependencies_[2].requires_token()),
  };
  EXPECT_TRUE(parent_koids.contains(FakeElementControl::GetKoid(parent_a->DuplicatePowerToken())));
  EXPECT_TRUE(parent_koids.contains(FakeElementControl::GetKoid(parent_b->DuplicatePowerToken())));
}

TEST_F(AllDriversElementTest, TestNodeRemovalPromotesGrandparentThroughUnboundParent) {
  auto usb_phy = CreateNode("usb-phy", root());
  auto dwc3_phy = CreateNode("dwc3-phy", usb_phy);
  auto dwc3 = CreateNode("dwc3", dwc3_phy);

  usb_phy->UpdateBoundStateForTesting(true);
  dwc3->UpdateBoundStateForTesting(true);

  AllDriversElement element(nullptr, root());
  SetElementForNodeManager(&element);
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetCurrentLevel(element, 1);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_EQ(FakeElementControl::GetKoid(*fake_control.added_dependencies_[0].requires_token()),
            FakeElementControl::GetKoid(dwc3->DuplicatePowerToken()));

  // Remove the unbound child node dwc3-phy (as usb-phy does when tearing down its child node).
  // When dwc3 finishes shutdown, OnNodeUnbinding must be called before dwc3-phy's parent link to
  // usb-phy is cleared, promoting usb-phy to a leaf node.
  dwc3_phy->Remove(RemovalSet::kAll, nullptr);
  RunLoopUntilIdle();

  ASSERT_EQ(fake_control.added_dependencies_.size(), 2u);
  EXPECT_EQ(FakeElementControl::GetKoid(*fake_control.added_dependencies_[1].requires_token()),
            FakeElementControl::GetKoid(usb_phy->DuplicatePowerToken()));
  SetElementForNodeManager(nullptr);
}

TEST_F(AllDriversElementTest, TestSubtreeRemovalSkipsShuttingDownIntermediateBoundNodes) {
  // Mirrors the full USB peripheral topology:
  // usb-phy (bound) -> dwc3-phy (unbound) -> dwc3-c400000 (bound) -> dwc3 (bound) ->
  // usb-peripheral (unbound) -> function-000 (bound).
  auto usb_phy = CreateNode("usb-phy", root());
  auto dwc3_phy = CreateNode("dwc3-phy", usb_phy);
  auto dwc3_core = CreateNode("dwc3-c400000", dwc3_phy);
  auto dwc3 = CreateNode("dwc3", dwc3_core);
  auto usb_peripheral = CreateNode("usb-peripheral", dwc3);
  auto function_000 = CreateNode("function-000", usb_peripheral);

  usb_phy->UpdateBoundStateForTesting(true);
  dwc3_core->UpdateBoundStateForTesting(true);
  dwc3->UpdateBoundStateForTesting(true);
  function_000->UpdateBoundStateForTesting(true);

  AllDriversElement element(nullptr, root());
  SetElementForNodeManager(&element);
  auto [control_client, control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                      fidl::kIgnoreBindingClosure);

  PowerElementHandles handles{
      .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
          std::move(control_client), dispatcher()),
  };
  element.AttachElement(dispatcher(), std::move(handles));
  SetCurrentLevel(element, 1);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_EQ(FakeElementControl::GetKoid(*fake_control.added_dependencies_[0].requires_token()),
            FakeElementControl::GetKoid(function_000->DuplicatePowerToken()));

  // Remove the entire dwc3-phy subtree. Intermediate bound nodes (dwc3, dwc3-c400000) are also
  // shutting down and must be skipped, promoting usb-phy directly.
  dwc3_phy->Remove(RemovalSet::kAll, nullptr);
  RunLoopUntilIdle();

  ASSERT_EQ(fake_control.added_dependencies_.size(), 2u);
  EXPECT_EQ(FakeElementControl::GetKoid(*fake_control.added_dependencies_[1].requires_token()),
            FakeElementControl::GetKoid(usb_phy->DuplicatePowerToken()));
  SetElementForNodeManager(nullptr);
}

}  // namespace
}  // namespace driver_manager
