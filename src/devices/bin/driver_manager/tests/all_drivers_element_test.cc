// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/power/all_drivers_element.h"

#include <fidl/fuchsia.power.broker/cpp/fidl.h>
#include <zircon/assert.h>

#include <map>
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
    if (add_dependency_error_.has_value()) {
      completer.Reply(fit::error(*add_dependency_error_));
      return;
    }
    // Like Power Broker, reject a dependency on an element that already has one. The required
    // element is identified by the koid of its power token.
    ZX_ASSERT(request.requires_token().has_value());
    zx_koid_t koid = GetKoid(*request.requires_token());
    if (!dependency_koids_.insert(koid).second) {
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

  void RemoveDependency(RemoveDependencyRequest& request,
                        RemoveDependencyCompleter::Sync& completer) override {
    ZX_ASSERT(request.requires_token().has_value());
    zx_koid_t koid = GetKoid(*request.requires_token());
    if (remove_dependency_error_.has_value() ||
        (fail_remove_dependency_koid_.has_value() && *fail_remove_dependency_koid_ == koid)) {
      completer.Reply(fit::error(remove_dependency_error_.value_or(
          fuchsia_power_broker::ModifyDependencyError::kNotAuthorized)));
      return;
    }
    // Like Power Broker, reject removing a dependency that the element does not have.
    if (dependency_koids_.erase(koid) == 0) {
      completer.Reply(fit::error(fuchsia_power_broker::ModifyDependencyError::kNotFound));
      return;
    }
    removed_dependencies_.push_back(std::move(request));
    if (auto_reply_remove_dependency_) {
      completer.Reply(fit::ok());
    } else {
      remove_dependency_completers_.push_back(completer.ToAsync());
    }
  }
  // Simulates Power Broker removing the dependency on the element that |token| belongs to. Power
  // Broker does this when the element is removed (e.g. when its node restarts), because the
  // dependency is added with |remove_with_required_element|.
  void RemoveDependencyOn(const zx::event& token) { dependency_koids_.erase(GetKoid(token)); }

  // Returns whether the element currently has an accepted dependency on the element that |token|
  // belongs to.
  bool HasDependencyOn(const zx::event& token) const {
    return dependency_koids_.contains(GetKoid(token));
  }

  void ReplyAddDependencies() {
    for (auto& comp : add_dependency_completers_) {
      comp.Reply(fit::ok());
    }
    add_dependency_completers_.clear();
  }

  void ReplyRemoveDependencies() {
    for (auto& comp : remove_dependency_completers_) {
      if (remove_dependency_error_.has_value()) {
        comp.Reply(fit::error(*remove_dependency_error_));
      } else {
        comp.Reply(fit::ok());
      }
    }
    remove_dependency_completers_.clear();
  }

  void OpenStatusChannel(OpenStatusChannelRequest& request,
                         OpenStatusChannelCompleter::Sync& completer) override {}

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_power_broker::ElementControl> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {}

  bool auto_reply_add_dependency_ = true;
  bool auto_reply_remove_dependency_ = true;
  std::optional<fuchsia_power_broker::ModifyDependencyError> add_dependency_error_;
  std::optional<fuchsia_power_broker::ModifyDependencyError> remove_dependency_error_;
  std::optional<zx_koid_t> fail_remove_dependency_koid_;
  std::vector<AddDependencyCompleter::Async> add_dependency_completers_;
  std::vector<RemoveDependencyCompleter::Async> remove_dependency_completers_;
  // The dependencies that were accepted, in the order they were added.
  std::vector<fuchsia_power_broker::LevelDependency> added_dependencies_;
  // The dependencies that were accepted for removal, in the order they were removed.
  std::vector<fuchsia_power_broker::LevelDependency> removed_dependencies_;
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

  static void ResetSessionToken(AllDriversElement& element,
                                const std::shared_ptr<const Node>& node) {
    auto it = element.suspended_sessions_.find(node.get());
    if (it != element.suspended_sessions_.end()) {
      it->second->token.reset();
    }
  }

  // Serves |fake_control| on |bindings| and attaches it to |element| as its Power Broker element
  // control, then sets the element to the "on" level. |fake_control| and |bindings| must be
  // declared after |element|, so that they are destroyed before it.
  void AttachFakeControl(AllDriversElement& element, FakeElementControl& fake_control,
                         fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl>& bindings) {
    auto [control_client, control_server] =
        fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
    bindings.AddBinding(dispatcher(), std::move(control_server), &fake_control,
                        fidl::kIgnoreBindingClosure);
    PowerElementHandles handles{
        .element_control = fidl::Client<fuchsia_power_broker::ElementControl>(
            std::move(control_client), dispatcher()),
    };
    element.AttachElement(std::move(handles));
    SetCurrentLevel(element, 1);
  }

  static fuchsia_driver_development::NodeTarget MakeTarget(std::string_view moniker) {
    return fuchsia_driver_development::NodeTarget{{.moniker = std::string(moniker)}};
  }

  // Suspends the driver matching |target| using a session token and runs the loop until idle.
  // Returns the result, or nullopt if the request has not completed. If |out_token| is provided,
  // the client token is moved into it; otherwise, the token is retained in |active_tokens_| keyed
  // by moniker.
  std::optional<zx::result<>> Suspend(AllDriversElement& element,
                                      const fuchsia_driver_development::NodeTarget& target,
                                      zx::eventpair* out_token = nullptr) {
    std::string token_key = target.moniker().value_or("");
    zx::eventpair client_token, server_token;
    zx_status_t status = zx::eventpair::create(0, &client_token, &server_token);
    ZX_ASSERT(status == ZX_OK);
    auto result = std::make_shared<std::optional<zx::result<>>>();
    element.StartSuspendSession(target, std::move(server_token),
                                [result](zx::result<> res) { *result = res; });
    RunLoopUntilIdle();
    if (out_token) {
      *out_token = std::move(client_token);
    } else if (!*result || (*result)->is_ok()) {
      active_tokens_[std::move(token_key)] = std::move(client_token);
    }
    return *result;
  }

  std::optional<zx::result<>> Suspend(AllDriversElement& element, std::string_view moniker,
                                      zx::eventpair* out_token = nullptr) {
    return Suspend(element, MakeTarget(moniker), out_token);
  }

  // Creates and retains a shared AllDriversElement instance bound to dispatcher().
  AllDriversElement& CreateElement(zx::eventpair bootup_lease = {}) {
    elements_.push_back(std::make_shared<AllDriversElement>(nullptr, root(),
                                                            std::move(bootup_lease), dispatcher()));
    return *elements_.back();
  }

  void TearDown() override {
    elements_.clear();
    DriverManagerTestBase::TearDown();
  }

  // Resumes the driver at |moniker| by dropping its active session token and running the loop
  // until idle.
  void Resume(std::string_view moniker) {
    auto it = active_tokens_.find(moniker);
    if (it != active_tokens_.end()) {
      it->second.reset();
      active_tokens_.erase(it);
    }
    RunLoopUntilIdle();
  }

 private:
  std::vector<std::shared_ptr<AllDriversElement>> elements_;
  std::map<std::string, zx::eventpair, std::less<>> active_tokens_;
  AllDriversTestNodeManager node_manager_;
};

}  // namespace driver_manager

namespace driver_manager {
namespace {

TEST_F(AllDriversElementTest, TestSetLevel) {
  AllDriversElement& element = CreateElement();
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

  element.AttachElement(std::move(handles));
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

  AllDriversElement& element = CreateElement();
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
  element.AttachElement(std::move(handles));
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

  AllDriversElement& element = CreateElement();
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
  element.AttachElement(std::move(handles));
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

  AllDriversElement& element = CreateElement();
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
  element.AttachElement(std::move(handles));
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
  AllDriversElement& element = CreateElement();
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
  element.AttachElement(std::move(handles));
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
  AllDriversElement& element = CreateElement();
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
  element.AttachElement(std::move(handles));
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

  AllDriversElement& element = CreateElement();
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
  element.AttachElement(std::move(handles));
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
  AllDriversElement& element = CreateElement();
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
  element.AttachElement(std::move(handles));
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
  AllDriversElement& element = CreateElement();
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
  element.AttachElement(std::move(handles));
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

  AllDriversElement& element = CreateElement();
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
  element.AttachElement(std::move(handles));
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
  AllDriversElement& element = CreateElement();
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
  element.AttachElement(std::move(handles));
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

  AllDriversElement& element = CreateElement(std::move(bootup_lease));
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
  element.AttachElement(std::move(handles));

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

TEST_F(AllDriversElementTest, TestStartSuspendSessionAndResume) {
  auto parent = CreateNode("parent", root());
  auto child = CreateNode("child", parent);
  auto unbound = CreateNode("unbound", root());

  parent->UpdateBoundStateForTesting(true);
  child->UpdateBoundStateForTesting(true);
  unbound->UpdateBoundStateForTesting(false);

  AllDriversElement& element = CreateElement();
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
  element.AttachElement(std::move(handles));
  SetCurrentLevel(element, 1);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);

  // Suspend driver at parent node. This removes the AllDrivers dependency on parent's dependent
  // (child).
  zx::eventpair client_token, server_token;
  ASSERT_EQ(zx::eventpair::create(0, &client_token, &server_token), ZX_OK);
  std::optional<zx::result<>> suspend_result;
  element.StartSuspendSession(MakeTarget("root.parent"), std::move(server_token),
                              [&suspend_result](zx::result<> res) { suspend_result = res; });
  RunLoopUntilIdle();

  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());
  ASSERT_EQ(fake_control.removed_dependencies_.size(), 1u);

  // Verify the removed dependency token matches child's power token (parent's dependent).
  zx::event expected_child_token = child->DuplicatePowerToken();
  zx_info_handle_basic_t actual_info, expected_info;
  ASSERT_EQ(fake_control.removed_dependencies_[0].requires_token()->get_info(
                ZX_INFO_HANDLE_BASIC, &actual_info, sizeof(actual_info), nullptr, nullptr),
            ZX_OK);
  ASSERT_EQ(expected_child_token.get_info(ZX_INFO_HANDLE_BASIC, &expected_info,
                                          sizeof(expected_info), nullptr, nullptr),
            ZX_OK);
  EXPECT_EQ(actual_info.koid, expected_info.koid);

  // Closing client_token resumes driver at parent node. Like the startup transition,
  // this adds the dependency on parent's bound leaf descendant (child), restoring what the suspend
  // removed. Parent is not a leaf, so it gets no dependency of its own.
  client_token.reset();
  RunLoopUntilIdle();

  ASSERT_EQ(fake_control.added_dependencies_.size(), 2u);
  ASSERT_EQ(fake_control.added_dependencies_[1].requires_token()->get_info(
                ZX_INFO_HANDLE_BASIC, &actual_info, sizeof(actual_info), nullptr, nullptr),
            ZX_OK);
  EXPECT_EQ(actual_info.koid, expected_info.koid);
  EXPECT_TRUE(fake_control.HasDependencyOn(expected_child_token));
  EXPECT_FALSE(fake_control.HasDependencyOn(parent->DuplicatePowerToken()));

  // Suspend driver at parent node again. The resume re-added child's dependency, so it is removed
  // again.
  zx::eventpair client_token2, server_token2;
  ASSERT_EQ(zx::eventpair::create(0, &client_token2, &server_token2), ZX_OK);
  suspend_result.reset();
  fake_control.removed_dependencies_.clear();
  element.StartSuspendSession(MakeTarget("root.parent"), std::move(server_token2),
                              [&suspend_result](zx::result<> res) { suspend_result = res; });
  RunLoopUntilIdle();

  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());
  ASSERT_EQ(fake_control.removed_dependencies_.size(), 1u);
  ASSERT_EQ(fake_control.removed_dependencies_[0].requires_token()->get_info(
                ZX_INFO_HANDLE_BASIC, &actual_info, sizeof(actual_info), nullptr, nullptr),
            ZX_OK);
  EXPECT_EQ(actual_info.koid, expected_info.koid);
  EXPECT_FALSE(fake_control.HasDependencyOn(expected_child_token));

  // Suspending a non-existent node returns ZX_ERR_NOT_FOUND.
  zx::eventpair client_token3, server_token3;
  ASSERT_EQ(zx::eventpair::create(0, &client_token3, &server_token3), ZX_OK);
  std::optional<zx::result<>> not_found_result;
  element.StartSuspendSession(MakeTarget("root.nonexistent"), std::move(server_token3),
                              [&not_found_result](zx::result<> res) { not_found_result = res; });
  RunLoopUntilIdle();
  ASSERT_TRUE(not_found_result.has_value());
  EXPECT_EQ(not_found_result->error_value(), ZX_ERR_NOT_FOUND);

  // Suspending with an empty moniker returns ZX_ERR_INVALID_ARGS.
  zx::eventpair client_token_empty, server_token_empty;
  ASSERT_EQ(zx::eventpair::create(0, &client_token_empty, &server_token_empty), ZX_OK);
  std::optional<zx::result<>> empty_path_result;
  element.StartSuspendSession(MakeTarget(""), std::move(server_token_empty),
                              [&empty_path_result](zx::result<> res) { empty_path_result = res; });
  RunLoopUntilIdle();
  ASSERT_TRUE(empty_path_result.has_value());
  EXPECT_EQ(empty_path_result->status_value(), ZX_ERR_INVALID_ARGS);

  // Suspending with a missing moniker returns ZX_ERR_INVALID_ARGS.
  zx::eventpair client_token_missing, server_token_missing;
  ASSERT_EQ(zx::eventpair::create(0, &client_token_missing, &server_token_missing), ZX_OK);
  std::optional<zx::result<>> missing_moniker_result;
  element.StartSuspendSession(
      fuchsia_driver_development::NodeTarget{}, std::move(server_token_missing),
      [&missing_moniker_result](zx::result<> res) { missing_moniker_result = res; });
  RunLoopUntilIdle();
  ASSERT_TRUE(missing_moniker_result.has_value());
  EXPECT_EQ(missing_moniker_result->status_value(), ZX_ERR_INVALID_ARGS);

  // Suspending with a moniker whose root doesn't match returns ZX_ERR_NOT_FOUND.
  zx::eventpair client_token_invalid_root, server_token_invalid_root;
  ASSERT_EQ(zx::eventpair::create(0, &client_token_invalid_root, &server_token_invalid_root),
            ZX_OK);
  std::optional<zx::result<>> invalid_root_result;
  element.StartSuspendSession(
      MakeTarget("not_root.child"), std::move(server_token_invalid_root),
      [&invalid_root_result](zx::result<> res) { invalid_root_result = res; });
  RunLoopUntilIdle();
  ASSERT_TRUE(invalid_root_result.has_value());
  EXPECT_EQ(invalid_root_result->error_value(), ZX_ERR_NOT_FOUND);

  // Suspending an unbound node returns ZX_ERR_BAD_STATE.
  zx::eventpair client_token4, server_token4;
  ASSERT_EQ(zx::eventpair::create(0, &client_token4, &server_token4), ZX_OK);
  std::optional<zx::result<>> unbound_result;
  element.StartSuspendSession(MakeTarget("root.unbound"), std::move(server_token4),
                              [&unbound_result](zx::result<> res) { unbound_result = res; });
  RunLoopUntilIdle();
  ASSERT_TRUE(unbound_result.has_value());
  EXPECT_EQ(unbound_result->error_value(), ZX_ERR_BAD_STATE);

  // Closing client_token2 resumes driver at parent node so that child has an active dependency.
  client_token2.reset();
  RunLoopUntilIdle();
  EXPECT_TRUE(element.HasDependency(child));
  EXPECT_TRUE(fake_control.HasDependencyOn(expected_child_token));

  // Suspending when Power Broker returns kNotAuthorized returns ZX_ERR_ACCESS_DENIED, and the
  // AllDriversElement dependency is not removed.
  fake_control.remove_dependency_error_ =
      fuchsia_power_broker::ModifyDependencyError::kNotAuthorized;
  zx::eventpair client_token5, server_token5;
  ASSERT_EQ(zx::eventpair::create(0, &client_token5, &server_token5), ZX_OK);
  std::optional<zx::result<>> access_denied_result;
  element.StartSuspendSession(
      MakeTarget("root.parent.child"), std::move(server_token5),
      [&access_denied_result](zx::result<> res) { access_denied_result = res; });
  RunLoopUntilIdle();
  ASSERT_TRUE(access_denied_result.has_value());
  EXPECT_EQ(access_denied_result->error_value(), ZX_ERR_ACCESS_DENIED);
  EXPECT_TRUE(element.HasDependency(child));
  EXPECT_TRUE(fake_control.HasDependencyOn(expected_child_token));
}

// Suspending a driver that is already suspended (directly or via an ancestor) returns
// ZX_ERR_ALREADY_EXISTS.
TEST_F(AllDriversElementTest, TestStartSuspendSessionFailsWhenSessionExists) {
  auto parent = CreateNode("parent", root());
  auto child = CreateNode("child", parent);
  for (const auto& node : {parent, child}) {
    node->UpdateBoundStateForTesting(true);
  }

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);

  auto suspend_child = Suspend(element, "root.parent.child");
  ASSERT_TRUE(suspend_child.has_value());
  EXPECT_TRUE(suspend_child->is_ok());

  // Suspending the same child again fails with ZX_ERR_ALREADY_EXISTS.
  auto suspend_child_again = Suspend(element, "root.parent.child");
  ASSERT_TRUE(suspend_child_again.has_value());
  EXPECT_TRUE(suspend_child_again->is_error());
  EXPECT_EQ(suspend_child_again->error_value(), ZX_ERR_ALREADY_EXISTS);

  // Resume child so we can test ancestor suspension.
  Resume("root.parent.child");

  // Suspend parent.
  auto suspend_parent = Suspend(element, "root.parent");
  ASSERT_TRUE(suspend_parent.has_value());
  EXPECT_TRUE(suspend_parent->is_ok());

  // Suspending child when its parent is already suspended fails with ZX_ERR_ALREADY_EXISTS.
  auto suspend_child_when_parent_suspended = Suspend(element, "root.parent.child");
  ASSERT_TRUE(suspend_child_when_parent_suspended.has_value());
  EXPECT_TRUE(suspend_child_when_parent_suspended->is_error());
  EXPECT_EQ(suspend_child_when_parent_suspended->error_value(), ZX_ERR_ALREADY_EXISTS);
}

// Resuming a driver at a leaf node adds back the dependency on the leaf itself.
TEST_F(AllDriversElementTest, TestResumeLeafDriver) {
  auto leaf = CreateNode("leaf", root());
  leaf->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_TRUE(fake_control.HasDependencyOn(leaf->DuplicatePowerToken()));

  auto suspend_result = Suspend(element, "root.leaf");
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());
  EXPECT_FALSE(fake_control.HasDependencyOn(leaf->DuplicatePowerToken()));

  Resume("root.leaf");
  EXPECT_TRUE(fake_control.HasDependencyOn(leaf->DuplicatePowerToken()));
  EXPECT_EQ(fake_control.added_dependencies_.size(), 2u);
}

// Models a touch driver whose node is a parent of a display composite:
//
//   root
//   |- touch
//   |  |- sensor
//   |  |- bridge (unbound)
//   |  |  `- deep_sensor
//   |  `- panel (composite of touch and other)
//   |     `- display
//   |- other
//   |  `- panel
//   `- standalone
//
// Suspending touch removes the AllDrivers dependencies on its bound leaf descendants (display,
// sensor and deep_sensor), which lets everything they were keeping powered, including touch itself,
// power down. Resuming touch must add exactly those dependencies back, like the startup transition
// did, so that all of it powers up again.
TEST_F(AllDriversElementTest, TestResumeRestoresDescendantLeaves) {
  auto touch = CreateNode("touch", root());
  auto other = CreateNode("other", root());
  auto standalone = CreateNode("standalone", root());
  auto sensor = CreateNode("sensor", touch);
  auto bridge = CreateNode("bridge", touch);
  auto deep_sensor = CreateNode("deep_sensor", bridge);
  std::vector<std::weak_ptr<Node>> panel_parents = {touch, other};
  auto panel = CreateCompositeNode("panel", panel_parents, {{}, {}});
  auto display = CreateNode("display", panel);

  for (const auto& node : {touch, other, standalone, sensor, deep_sensor, panel, display}) {
    node->UpdateBoundStateForTesting(true);
  }
  bridge->UpdateBoundStateForTesting(false);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  auto has_dependency = [&fake_control](const std::shared_ptr<Node>& node) {
    return fake_control.HasDependencyOn(node->DuplicatePowerToken());
  };

  // Only the leaves depend on AllDrivers.
  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 4u);
  EXPECT_TRUE(has_dependency(display));
  EXPECT_TRUE(has_dependency(sensor));
  EXPECT_TRUE(has_dependency(deep_sensor));
  EXPECT_TRUE(has_dependency(standalone));

  auto suspend_result = Suspend(element, "root.touch");
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());
  EXPECT_EQ(fake_control.removed_dependencies_.size(), 3u);
  EXPECT_FALSE(has_dependency(display));
  EXPECT_FALSE(has_dependency(sensor));
  EXPECT_FALSE(has_dependency(deep_sensor));
  // External composite parent |other| is kept powered while |panel|/|display| are suspended.
  EXPECT_TRUE(has_dependency(other));
  // The leaf outside of touch's subtree is not affected.
  EXPECT_TRUE(has_dependency(standalone));

  Resume("root.touch");

  // Every suspended leaf has its dependency back.
  EXPECT_TRUE(has_dependency(display));
  EXPECT_TRUE(has_dependency(sensor));
  EXPECT_TRUE(has_dependency(deep_sensor));
  EXPECT_TRUE(has_dependency(standalone));
  EXPECT_EQ(fake_control.added_dependencies_.size(), 8u);
  // Nothing else keeps a dependency. The leaves keep the nodes above them powered.
  EXPECT_FALSE(has_dependency(touch));
  EXPECT_FALSE(has_dependency(other));
  EXPECT_FALSE(has_dependency(panel));
  EXPECT_FALSE(has_dependency(bridge));
  // Resuming did not try to add the dependency that was still in place on standalone.
  EXPECT_EQ(fake_control.already_exists_count_, 0u);
}

// Like the startup transition, resuming does not add a dependency on a hermetic power test node.
TEST_F(AllDriversElementTest, TestResumeSkipsHermeticPowerTestNodes) {
  std::map<std::string, zx::event> token_overrides;
  ASSERT_EQ(zx::event::create(0, &token_overrides["hermetic"]), ZX_OK);

  auto parent = CreateNode("parent", root());
  auto hermetic = CreateNode("hermetic", parent, std::move(token_overrides));
  auto regular = CreateNode("regular", parent);
  for (const auto& node : {parent, hermetic, regular}) {
    node->UpdateBoundStateForTesting(true);
  }
  ASSERT_TRUE(hermetic->IsHermeticPowerTest());

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  ASSERT_TRUE(fake_control.HasDependencyOn(regular->DuplicatePowerToken()));

  auto suspend_result = Suspend(element, "root.parent");
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());
  EXPECT_FALSE(fake_control.HasDependencyOn(regular->DuplicatePowerToken()));

  Resume("root.parent");
  EXPECT_TRUE(fake_control.HasDependencyOn(regular->DuplicatePowerToken()));
  EXPECT_FALSE(fake_control.HasDependencyOn(hermetic->DuplicatePowerToken()));
  EXPECT_EQ(fake_control.added_dependencies_.size(), 2u);
}

// Suspending directly on a hermetic power test node returns ZX_ERR_NOT_SUPPORTED because
// hermetic power test nodes do not participate in AllDrivers power management. If a non-hermetic
// node has no valid leaf driver nodes in its subtree, it returns ZX_ERR_INTERNAL.
TEST_F(AllDriversElementTest, TestStartSuspendSessionFailsWhenNoLeafDriverNodes) {
  std::map<std::string, zx::event> token_overrides;
  ASSERT_EQ(zx::event::create(0, &token_overrides["hermetic"]), ZX_OK);

  auto parent = CreateNode("parent", root());
  auto hermetic = CreateNode("hermetic", parent, std::move(token_overrides));
  parent->UpdateBoundStateForTesting(true);
  hermetic->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  // Suspending a hermetic node directly returns ZX_ERR_NOT_SUPPORTED.
  auto hermetic_suspend = Suspend(element, "root.parent.hermetic");
  ASSERT_TRUE(hermetic_suspend.has_value());
  EXPECT_TRUE(hermetic_suspend->is_error());
  EXPECT_EQ(hermetic_suspend->error_value(), ZX_ERR_NOT_SUPPORTED);

  // Suspending parent, whose only bound descendant is hermetic (so no non-hermetic leaf exists),
  // returns ZX_ERR_INTERNAL.
  auto parent_suspend = Suspend(element, "root.parent");
  ASSERT_TRUE(parent_suspend.has_value());
  EXPECT_TRUE(parent_suspend->is_error());
  EXPECT_EQ(parent_suspend->error_value(), ZX_ERR_INTERNAL);
}

// Suspending reports a failure to remove a dependency, and does not remove dependencies.
TEST_F(AllDriversElementTest, TestStartSuspendSessionReportsRemoveDependencyErrors) {
  auto parent = CreateNode("parent", root());
  auto child_a = CreateNode("child_a", parent);
  auto child_b = CreateNode("child_b", parent);
  for (const auto& node : {parent, child_a, child_b}) {
    node->UpdateBoundStateForTesting(true);
  }

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  struct TestCase {
    fuchsia_power_broker::ModifyDependencyError error;
    zx_status_t expected_error;
  };
  const TestCase kTestCases[] = {
      {fuchsia_power_broker::ModifyDependencyError::kNotAuthorized, ZX_ERR_ACCESS_DENIED},
      {fuchsia_power_broker::ModifyDependencyError::kNotFound, ZX_ERR_NOT_FOUND},
      {fuchsia_power_broker::ModifyDependencyError::kInvalid, ZX_ERR_INTERNAL},
  };
  for (const auto& test_case : kTestCases) {
    fake_control.remove_dependency_error_ = test_case.error;
    size_t callback_count = 0;
    std::optional<zx::result<>> result;
    zx::eventpair client_token, server_token;
    ASSERT_EQ(zx::eventpair::create(0, &client_token, &server_token), ZX_OK);
    element.StartSuspendSession(MakeTarget("root.parent"), std::move(server_token),
                                [&](zx::result<> res) {
                                  callback_count++;
                                  result = res;
                                });
    RunLoopUntilIdle();

    EXPECT_EQ(callback_count, 1u);
    ASSERT_TRUE(result.has_value());
    EXPECT_TRUE(result->is_error());
    EXPECT_EQ(result->error_value(), test_case.expected_error);
    EXPECT_TRUE(element.HasDependency(child_a));
    EXPECT_TRUE(element.HasDependency(child_b));
  }
}

// When suspending a subtree where one leaf succeeds in removing its dependency but another fails
// (e.g. due to not authorized), the removed dependency is rolled back (re-added), and the
// AllDrivers dependencies are retained.
TEST_F(AllDriversElementTest, TestSuspendNotAuthorizedRollsBackRemovedDependencies) {
  auto parent = CreateNode("parent", root());
  auto child_a = CreateNode("child_a", parent);
  auto child_b = CreateNode("child_b", parent);
  for (const auto& node : {parent, child_a, child_b}) {
    node->UpdateBoundStateForTesting(true);
  }

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  zx::event token_a = child_a->DuplicatePowerToken();
  zx::event token_b = child_b->DuplicatePowerToken();
  EXPECT_TRUE(element.HasDependency(child_a));
  EXPECT_TRUE(element.HasDependency(child_b));
  EXPECT_TRUE(fake_control.HasDependencyOn(token_a));
  EXPECT_TRUE(fake_control.HasDependencyOn(token_b));

  // Configure child_b to fail removal with kNotAuthorized.
  fake_control.fail_remove_dependency_koid_ = FakeElementControl::GetKoid(token_b);

  zx::eventpair client_token, server_token;
  ASSERT_EQ(zx::eventpair::create(0, &client_token, &server_token), ZX_OK);
  std::optional<zx::result<>> result;
  element.StartSuspendSession(MakeTarget("root.parent"), std::move(server_token),
                              [&](zx::result<> res) { result = res; });
  RunLoopUntilIdle();
  RunLoopUntilIdle();

  ASSERT_TRUE(result.has_value());
  EXPECT_TRUE(result->is_error());
  EXPECT_EQ(result->error_value(), ZX_ERR_ACCESS_DENIED);

  // Both child_a and child_b must retain their dependencies.
  EXPECT_TRUE(element.HasDependency(child_a));
  EXPECT_TRUE(element.HasDependency(child_b));
  EXPECT_TRUE(fake_control.HasDependencyOn(token_a));
  EXPECT_TRUE(fake_control.HasDependencyOn(token_b));
}

// Resuming completes once Power Broker has replied to the request for every leaf.
TEST_F(AllDriversElementTest, TestResumeCompletesAfterAllDependenciesAreAdded) {
  auto parent = CreateNode("parent", root());
  auto child_a = CreateNode("child_a", parent);
  auto child_b = CreateNode("child_b", parent);
  for (const auto& node : {parent, child_a, child_b}) {
    node->UpdateBoundStateForTesting(true);
  }

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  zx::eventpair token;
  auto suspend_result = Suspend(element, "root.parent", &token);
  ASSERT_TRUE(suspend_result.has_value());
  ASSERT_EQ(fake_control.removed_dependencies_.size(), 2u);

  fake_control.auto_reply_add_dependency_ = false;
  token.reset();
  RunLoopUntilIdle();

  // Both requests were sent, but Power Broker has not replied to either of them yet.
  EXPECT_EQ(fake_control.added_dependencies_.size(), 4u);
  EXPECT_EQ(fake_control.add_dependency_completers_.size(), 2u);

  fake_control.ReplyAddDependencies();
  RunLoopUntilIdle();

  EXPECT_TRUE(fake_control.add_dependency_completers_.empty());
  EXPECT_TRUE(element.HasDependency(child_a));
  EXPECT_TRUE(element.HasDependency(child_b));
}

TEST_F(AllDriversElementTest, TestOnNodeUnbindingPromotesParentToLeaf) {
  auto parent = CreateNode("parent", root());
  auto child = CreateNode("child", parent);

  parent->UpdateBoundStateForTesting(true);
  child->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
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
  element.AttachElement(std::move(handles));
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

  AllDriversElement& element = CreateElement();
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
  element.AttachElement(std::move(handles));
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

  AllDriversElement& element = CreateElement();
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
  element.AttachElement(std::move(handles));
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

  AllDriversElement& element = CreateElement();
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
  element.AttachElement(std::move(handles));
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

  AllDriversElement& element = CreateElement();
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
  element.AttachElement(std::move(handles));
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

  AllDriversElement& element = CreateElement();
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
  element.AttachElement(std::move(handles));
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

TEST_F(AllDriversElementTest, TestSuspendedLeafRestartCancelsSuspendSession) {
  auto leaf = CreateNode("leaf", root());
  leaf->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_TRUE(element.HasDependency(leaf));

  // Suspend the leaf node.
  zx::eventpair token;
  auto suspend_result = Suspend(element, "root.leaf", &token);
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());
  EXPECT_EQ(fake_control.removed_dependencies_.size(), 1u);
  EXPECT_FALSE(element.HasDependency(leaf));
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(leaf));

  // Simulate leaf driver restarting while suspended.
  // During a restart, the driver unbinds first before binding again.
  // Unbinding cancels the active suspend session before the node binds again.
  leaf->UpdateBoundStateForTesting(false);
  element.OnNodeUnbinding(leaf);
  RunLoopUntilIdle();

  // The suspend session token peer is closed upon unbinding.
  zx_signals_t token_pending = 0;
  EXPECT_EQ(token.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &token_pending),
            ZX_OK);
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(leaf));

  // The restarted leaf driver now binds again.
  leaf->UpdateBoundStateForTesting(true);
  zx::eventpair lease, lease_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease, &lease_peer), ZX_OK);
  leaf->set_startup_lease_for_testing(std::move(lease));

  element.OnNodeBound(leaf);
  RunLoopUntilIdle();

  // OnNodeBound adds the dependency again and drops the startup lease.
  EXPECT_EQ(fake_control.added_dependencies_.size(), 2u);
  EXPECT_TRUE(element.HasDependency(leaf));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(leaf));

  zx_signals_t pending = 0;
  EXPECT_EQ(lease_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);
}

TEST_F(AllDriversElementTest, TestSuspendedParentDescendantRestartCancelsSuspendSession) {
  auto parent = CreateNode("parent", root());
  auto child = CreateNode("child", parent);
  parent->UpdateBoundStateForTesting(true);
  child->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_TRUE(element.HasDependency(child));

  // Suspend the parent node, which removes dependencies on its leaf descendant (child).
  zx::eventpair token;
  auto suspend_result = Suspend(element, "root.parent", &token);
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());
  EXPECT_EQ(fake_control.removed_dependencies_.size(), 1u);
  EXPECT_FALSE(element.HasDependency(child));
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(parent));
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(child));

  // Simulate child restarting while its ancestor is suspended.
  child->UpdateBoundStateForTesting(false);
  element.OnNodeUnbinding(child);
  RunLoopUntilIdle();

  // Child unbinding does not cancel parent's suspend session.
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(parent));
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(child));

  child->UpdateBoundStateForTesting(true);
  zx::eventpair child_lease, child_lease_peer;
  ASSERT_EQ(zx::eventpair::create(0, &child_lease, &child_lease_peer), ZX_OK);
  child->set_startup_lease_for_testing(std::move(child_lease));

  element.OnNodeBound(child);
  RunLoopUntilIdle();

  // Child is not the target of the suspend session; its ancestor is suspended, so child
  // remains suspended and drops its lease without adding a dependency.
  EXPECT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_FALSE(element.HasDependency(child));
  auto taken_child_lease = child->TakeStartupLease();
  EXPECT_TRUE(!taken_child_lease.has_value() || !taken_child_lease->is_valid());
  zx_signals_t child_pending = 0;
  EXPECT_EQ(child_lease_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(),
                                      &child_pending),
            ZX_OK);

  // Simulate parent (target node) restarting while suspended.
  // Parent unbinds first before binding again.
  parent->UpdateBoundStateForTesting(false);
  element.OnNodeUnbinding(parent);
  RunLoopUntilIdle();

  // Parent is the target node of the suspend session. When it unbinds, its suspend session
  // is cancelled before it binds again, restoring power dependencies for its bound leaf descendants
  // (child).
  EXPECT_EQ(fake_control.added_dependencies_.size(), 2u);
  EXPECT_TRUE(element.HasDependency(child));
  EXPECT_FALSE(element.HasDependency(parent));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(parent));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(child));

  // The suspend session token peer is closed because the session was cancelled.
  zx_signals_t token_pending = 0;
  EXPECT_EQ(token.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &token_pending),
            ZX_OK);

  // Parent binds again.
  parent->UpdateBoundStateForTesting(true);
  zx::eventpair parent_lease, parent_lease_peer;
  ASSERT_EQ(zx::eventpair::create(0, &parent_lease, &parent_lease_peer), ZX_OK);
  parent->set_startup_lease_for_testing(std::move(parent_lease));

  element.OnNodeBound(parent);
  RunLoopUntilIdle();

  // Parent has a power-enabled descendant (child), so parent drops its lease and adds no
  // dependency.
  EXPECT_EQ(fake_control.added_dependencies_.size(), 2u);
  EXPECT_TRUE(element.HasDependency(child));
  EXPECT_FALSE(element.HasDependency(parent));

  auto taken_parent_lease = parent->TakeStartupLease();
  EXPECT_TRUE(!taken_parent_lease.has_value() || !taken_parent_lease->is_valid());
  zx_signals_t parent_pending = 0;
  EXPECT_EQ(parent_lease_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(),
                                       &parent_pending),
            ZX_OK);
}

TEST_F(AllDriversElementTest, TestSuspendedLeafUnbindingPromotesParentToLeaf) {
  auto parent = CreateNode("parent", root());
  auto child = CreateNode("child", parent);
  parent->UpdateBoundStateForTesting(true);
  child->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_TRUE(element.HasDependency(child));

  // Suspend the child node.
  auto suspend_result = Suspend(element, "root.parent.child");
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());
  EXPECT_EQ(fake_control.removed_dependencies_.size(), 1u);
  EXPECT_FALSE(element.HasDependency(child));
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(child));

  // Child unbinds. Since child was a leaf and the target of a suspend session,
  // unbinding it cancels its session and promotes parent to a leaf.
  child->UpdateBoundStateForTesting(false);
  element.OnNodeUnbinding(child);
  RunLoopUntilIdle();

  EXPECT_EQ(fake_control.added_dependencies_.size(), 2u);
  EXPECT_TRUE(element.HasDependency(parent));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(child));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(parent));
}

// When a descendant node is suspended and then an ancestor is suspended, StartSuspendSession
// skips the already-suspended descendant and removes dependencies for the remaining active leaves.
// Suspending the ancestor again fails with ZX_ERR_ALREADY_EXISTS.
TEST_F(AllDriversElementTest, TestStartSuspendSessionOverlappingSubtrees) {
  auto parent = CreateNode("parent", root());
  auto child_a = CreateNode("child_a", parent);
  auto child_b = CreateNode("child_b", parent);
  parent->UpdateBoundStateForTesting(true);
  child_a->UpdateBoundStateForTesting(true);
  child_b->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 2u);
  EXPECT_TRUE(element.HasDependency(child_a));
  EXPECT_TRUE(element.HasDependency(child_b));

  // Suspend child_a first.
  auto suspend_child_a = Suspend(element, "root.parent.child_a");
  ASSERT_TRUE(suspend_child_a.has_value());
  EXPECT_TRUE(suspend_child_a->is_ok());
  EXPECT_EQ(fake_control.removed_dependencies_.size(), 1u);
  EXPECT_FALSE(element.HasDependency(child_a));
  EXPECT_TRUE(element.HasDependency(child_b));

  // Now suspend parent. Since child_a is already suspended (no dependency),
  // StartSuspendSession only removes child_b and does not fail with ZX_ERR_NOT_FOUND on child_a.
  auto suspend_parent = Suspend(element, "root.parent");
  ASSERT_TRUE(suspend_parent.has_value());
  EXPECT_TRUE(suspend_parent->is_ok());
  EXPECT_EQ(fake_control.removed_dependencies_.size(), 2u);
  EXPECT_FALSE(element.HasDependency(child_a));
  EXPECT_FALSE(element.HasDependency(child_b));
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(parent));

  // Suspending parent again fails with ZX_ERR_ALREADY_EXISTS.
  auto suspend_parent_again = Suspend(element, "root.parent");
  ASSERT_TRUE(suspend_parent_again.has_value());
  EXPECT_TRUE(suspend_parent_again->is_error());
  EXPECT_EQ(suspend_parent_again->error_value(), ZX_ERR_ALREADY_EXISTS);
  EXPECT_EQ(fake_control.removed_dependencies_.size(), 2u);
}

TEST_F(AllDriversElementTest, TestStartSuspendSessionInFlightNodeBoundDropsLease) {
  auto parent = CreateNode("parent", root());
  auto child_a = CreateNode("child_a", parent);
  parent->UpdateBoundStateForTesting(true);
  child_a->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fake_control.auto_reply_remove_dependency_ = false;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_TRUE(element.HasDependency(child_a));

  // StartSuspendSession on parent begins, dispatching RemoveDependency for child_a.
  zx::eventpair client_token, server_token;
  ASSERT_EQ(zx::eventpair::create(0, &client_token, &server_token), ZX_OK);
  bool suspend_completed = false;
  element.StartSuspendSession(MakeTarget("root.parent"), std::move(server_token),
                              [&](zx::result<> res) {
                                EXPECT_TRUE(res.is_ok());
                                suspend_completed = true;
                              });
  RunLoopUntilIdle();

  // The suspend is still in flight (auto_reply_remove_dependency_ is false).
  EXPECT_FALSE(suspend_completed);
  EXPECT_EQ(fake_control.remove_dependency_completers_.size(), 1u);
  // parent should already be considered suspended.
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(parent));

  // While suspend is in-flight, a new node (child_b) binds under parent.
  auto child_b = CreateNode("child_b", parent);
  child_b->UpdateBoundStateForTesting(true);
  zx::eventpair lease_b, lease_b_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease_b, &lease_b_peer), ZX_OK);
  child_b->set_startup_lease_for_testing(std::move(lease_b));

  element.OnNodeBound(child_b);
  RunLoopUntilIdle();

  // OnNodeBound must recognize parent is suspended, drop the lease, and NOT add a dependency.
  EXPECT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_FALSE(element.HasDependency(child_b));
  auto taken_lease = child_b->TakeStartupLease();
  EXPECT_TRUE(!taken_lease.has_value() || !taken_lease->is_valid());
  zx_signals_t pending = 0;
  EXPECT_EQ(lease_b_peer.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), &pending),
            ZX_OK);

  // Complete the in-flight RemoveDependency call.
  fake_control.ReplyRemoveDependencies();
  RunLoopUntilIdle();

  EXPECT_TRUE(suspend_completed);
  EXPECT_FALSE(element.HasDependency(child_a));
  EXPECT_FALSE(element.HasDependency(child_b));
}

TEST_F(AllDriversElementTest, TestResumeInFlightNodeBoundAddsDependency) {
  auto parent = CreateNode("parent", root());
  auto child_a = CreateNode("child_a", parent);
  parent->UpdateBoundStateForTesting(true);
  child_a->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_TRUE(element.HasDependency(child_a));

  // Suspend parent node, removing dependency on child_a.
  zx::eventpair token;
  auto suspend_result = Suspend(element, "root.parent", &token);
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());
  EXPECT_FALSE(element.HasDependency(child_a));
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(parent));

  // Disable auto-reply for AddDependency before resuming, keeping Resume in flight.
  fake_control.auto_reply_add_dependency_ = false;

  token.reset();
  RunLoopUntilIdle();

  // Resume is still in flight.
  EXPECT_EQ(fake_control.add_dependency_completers_.size(), 1u);
  // parent should already be considered resumed (no longer in suspended_sessions_).
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(parent));

  // While resume is in-flight, a new node (child_b) binds under parent.
  auto child_b = CreateNode("child_b", parent);
  child_b->UpdateBoundStateForTesting(true);
  zx::eventpair lease_b, lease_b_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease_b, &lease_b_peer), ZX_OK);
  child_b->set_startup_lease_for_testing(std::move(lease_b));

  element.OnNodeBound(child_b);
  RunLoopUntilIdle();

  // OnNodeBound must recognize parent is no longer suspended and dispatch AddDependency for
  // child_b. There are now 2 pending AddDependency completers (child_a and child_b).
  EXPECT_EQ(fake_control.add_dependency_completers_.size(), 2u);

  // Complete both in-flight AddDependency calls.
  fake_control.ReplyAddDependencies();
  RunLoopUntilIdle();

  EXPECT_TRUE(element.HasDependency(child_a));
  EXPECT_TRUE(element.HasDependency(child_b));
}

TEST_F(AllDriversElementTest, TestStartSuspendSessionRollbackRestoresInFlightBoundNodes) {
  auto parent = CreateNode("parent", root());
  auto child_a = CreateNode("child_a", parent);
  parent->UpdateBoundStateForTesting(true);
  child_a->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fake_control.auto_reply_remove_dependency_ = false;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_TRUE(element.HasDependency(child_a));

  // StartSuspendSession on parent begins, dispatching RemoveDependency for child_a.
  zx::eventpair client_token, server_token;
  ASSERT_EQ(zx::eventpair::create(0, &client_token, &server_token), ZX_OK);
  std::optional<zx::result<>> suspend_result;
  element.StartSuspendSession(MakeTarget("root.parent"), std::move(server_token),
                              [&](zx::result<> res) { suspend_result = res; });
  RunLoopUntilIdle();

  // The suspend is still in flight.
  EXPECT_FALSE(suspend_result.has_value());
  EXPECT_EQ(fake_control.remove_dependency_completers_.size(), 1u);
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(parent));

  // While suspend is in-flight, child_b binds under parent.
  auto child_b = CreateNode("child_b", parent);
  child_b->UpdateBoundStateForTesting(true);
  zx::eventpair lease_b, lease_b_peer;
  ASSERT_EQ(zx::eventpair::create(0, &lease_b, &lease_b_peer), ZX_OK);
  child_b->set_startup_lease_for_testing(std::move(lease_b));

  element.OnNodeBound(child_b);
  RunLoopUntilIdle();

  // OnNodeBound dropped child_b's lease and did NOT add a dependency because parent is suspended.
  EXPECT_FALSE(element.HasDependency(child_b));

  // Now, the in-flight RemoveDependency call fails.
  fake_control.remove_dependency_error_ = fuchsia_power_broker::ModifyDependencyError::kInvalid;
  fake_control.ReplyRemoveDependencies();
  RunLoopUntilIdle();

  // Suspend should have failed.
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_error());
  EXPECT_EQ(suspend_result->error_value(), ZX_ERR_INTERNAL);

  // Rollback via Resume() must have re-scanned the subtree and restored dependencies for
  // both child_a and newly-bound child_b.
  EXPECT_TRUE(element.HasDependency(child_a));
  EXPECT_TRUE(element.HasDependency(child_b));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(parent));
}

TEST_F(AllDriversElementTest, TestStartSuspendSessionRollbackOnFailureClearsSuspendedPath) {
  auto leaf = CreateNode("leaf", root());
  leaf->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fake_control.remove_dependency_error_ = fuchsia_power_broker::ModifyDependencyError::kInvalid;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);

  auto suspend_result = Suspend(element, "root.leaf");
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_error());
  EXPECT_EQ(suspend_result->error_value(), ZX_ERR_INTERNAL);
  // Suspended path must be cleared on failure.
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(leaf));
}

TEST_F(AllDriversElementTest, TestStartSuspendSessionBeforeStartupTransitionReturnsUnavailable) {
  auto leaf = CreateNode("leaf", root());
  leaf->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  // Calling StartSuspendSession before CompleteStartupTransition() must return ZX_ERR_UNAVAILABLE.
  auto suspend_result = Suspend(element, "root.leaf");
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_error());
  EXPECT_EQ(suspend_result->error_value(), ZX_ERR_UNAVAILABLE);
}

// Verifies that during suspend rollback, the caller's callback is deferred until all rollback
// AddDependency calls complete.
TEST_F(AllDriversElementTest, TestSuspendRollbackDefersCallbackUntilAddDependencyCompletes) {
  auto parent = CreateNode("parent", root());
  auto child_a = CreateNode("child_a", parent);
  auto child_b = CreateNode("child_b", parent);
  for (const auto& node : {parent, child_a, child_b}) {
    node->UpdateBoundStateForTesting(true);
  }

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  zx::event token_b = child_b->DuplicatePowerToken();
  fake_control.fail_remove_dependency_koid_ = FakeElementControl::GetKoid(token_b);

  // Disable auto-reply for AddDependency before suspending, so that rollback AddDependency calls
  // remain in flight.
  fake_control.auto_reply_add_dependency_ = false;

  zx::eventpair client_token, server_token;
  ASSERT_EQ(zx::eventpair::create(0, &client_token, &server_token), ZX_OK);
  std::optional<zx::result<>> result;
  element.StartSuspendSession(MakeTarget("root.parent"), std::move(server_token),
                              [&](zx::result<> res) { result = res; });
  RunLoopUntilIdle();

  // The callback must NOT have completed yet because AddDependency during rollback is pending.
  EXPECT_FALSE(result.has_value());

  // Complete the rollback AddDependency call.
  fake_control.ReplyAddDependencies();
  RunLoopUntilIdle();

  ASSERT_TRUE(result.has_value());
  EXPECT_TRUE(result->is_error());
  EXPECT_EQ(result->error_value(), ZX_ERR_ACCESS_DENIED);
  EXPECT_TRUE(element.HasDependency(child_a));
  EXPECT_TRUE(element.HasDependency(child_b));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(parent));
}

TEST_F(AllDriversElementTest, TestIsNodeOrAncestorSuspendedCompositeDAG) {
  auto primary_parent = CreateNode("primary_parent", root());
  auto secondary_grandparent = CreateNode("secondary_grandparent", root());
  auto secondary_parent = CreateNode("secondary_parent", secondary_grandparent);
  std::vector<std::weak_ptr<Node>> parents = {primary_parent, secondary_parent};
  auto composite = CreateCompositeNode("composite", parents, {{}, {}});
  auto child = CreateNode("child", composite);

  for (const auto& node :
       {primary_parent, secondary_grandparent, secondary_parent, composite, child}) {
    node->UpdateBoundStateForTesting(true);
  }

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  // Initially nothing is suspended.
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(composite));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(child));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(primary_parent));

  // Case 1: Suspend secondary_parent directly (non-primary parent).
  auto suspend_result = Suspend(element, "root.secondary_grandparent.secondary_parent");
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());

  // Both composite and child should be recognized as suspended via secondary branch.
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(secondary_parent));
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(composite));
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(child));
  // primary_parent is not suspended.
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(primary_parent));

  // Resume secondary_parent.
  Resume("root.secondary_grandparent.secondary_parent");

  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(composite));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(child));

  // Case 2: Suspend secondary_grandparent (multi-hop on non-primary branch).
  suspend_result = Suspend(element, "root.secondary_grandparent");
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());

  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(secondary_grandparent));
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(secondary_parent));
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(composite));
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(child));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(primary_parent));

  // Resume secondary_grandparent.
  Resume("root.secondary_grandparent");

  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(composite));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(child));

  // Case 3: Suspend primary_parent (covers starts_with check without DAG walk).
  suspend_result = Suspend(element, "root.primary_parent");
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());

  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(primary_parent));
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(composite));
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(child));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(secondary_parent));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(secondary_grandparent));
}

// Verifies that if a child node is currently unbound / mid-restart when suspend is
// requested on the parent, GetSubtreeLeafNodes reports ZX_ERR_BAD_STATE rather than
// ZX_ERR_INTERNAL.
TEST_F(AllDriversElementTest, TestStartSuspendSessionMidRestartChildReturnsBadState) {
  auto parent = CreateNode("parent", root());
  auto child = CreateNode("child", parent);
  parent->UpdateBoundStateForTesting(true);
  child->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);

  // Simulate child node being unbound / stopping mid-restart.
  child->UpdateBoundStateForTesting(false);

  // Calling StartSuspendSession on parent while child is mid-restart returns ZX_ERR_BAD_STATE.
  auto suspend_result = Suspend(element, "root.parent");
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_error());
  EXPECT_EQ(suspend_result->error_value(), ZX_ERR_BAD_STATE);
}

// Verifies that a driver with an unbound child service node (which has no power element and no
// dependency) can still be suspended and resumed, and the unbound child does not cause
// ZX_ERR_BAD_STATE.
TEST_F(AllDriversElementTest, TestStartSuspendSessionDriverWithUnboundServiceChildSucceeds) {
  auto parent = CreateNode("parent", root());
  auto service_child = CreateNode("service_child", parent);
  parent->UpdateBoundStateForTesting(true);
  // service_child is unbound and has no power element.
  service_child->UpdateBoundStateForTesting(false, false);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_EQ(FakeElementControl::GetKoid(*fake_control.added_dependencies_[0].requires_token()),
            FakeElementControl::GetKoid(parent->DuplicatePowerToken()));

  // Suspend parent driver.
  auto suspend_result = Suspend(element, "root.parent");
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(parent));

  // Resume parent driver.
  Resume("root.parent");
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(parent));
}

TEST_F(AllDriversElementTest, TestSuspendSessionTokenDroppedResumesDriver) {
  auto parent = CreateNode("parent", root());
  auto child = CreateNode("child", parent);
  parent->UpdateBoundStateForTesting(true);
  child->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_TRUE(element.HasDependency(child));

  zx::eventpair client_token, server_token;
  ASSERT_EQ(zx::eventpair::create(0, &client_token, &server_token), ZX_OK);

  std::optional<zx::result<>> suspend_result;
  element.StartSuspendSession(MakeTarget("root.parent"), std::move(server_token),
                              [&](zx::result<> res) { suspend_result = res; });
  RunLoopUntilIdle();

  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());
  EXPECT_FALSE(element.HasDependency(child));
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(parent));

  // Dropping the client token triggers the wait handler to resume the subtree.
  client_token.reset();
  RunLoopUntilIdle();

  EXPECT_TRUE(element.HasDependency(child));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(parent));
  EXPECT_EQ(fake_control.added_dependencies_.size(), 2u);
}

TEST_F(AllDriversElementTest, TestSuspendRejectsInvalidToken) {
  auto leaf = CreateNode("leaf", root());
  leaf->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  std::optional<zx::result<>> suspend_result;
  element.StartSuspendSession(MakeTarget("root.leaf"), zx::eventpair(),
                              [&](zx::result<> res) { suspend_result = res; });
  RunLoopUntilIdle();

  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_error());
  EXPECT_EQ(suspend_result->error_value(), ZX_ERR_BAD_HANDLE);
}

// If arming the async wait on the session token fails (e.g. token handle is invalid),
// CompleteSuspend logs an error, rolls back any removed dependencies by re-adding them, and returns
// ZX_ERR_INTERNAL.
TEST_F(AllDriversElementTest, TestStartSuspendSessionRollsBackWhenWaitBeginFails) {
  auto leaf = CreateNode("leaf", root());
  leaf->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fake_control.auto_reply_remove_dependency_ = false;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  EXPECT_TRUE(element.HasDependency(leaf));

  zx::eventpair client_token, server_token;
  ASSERT_EQ(zx::eventpair::create(0, &client_token, &server_token), ZX_OK);

  std::optional<zx::result<>> suspend_result;
  element.StartSuspendSession(MakeTarget("root.leaf"), std::move(server_token),
                              [&](zx::result<> res) { suspend_result = res; });
  RunLoopUntilIdle();

  // Reset the server token handle in the active session so Begin() will fail with
  // ZX_ERR_BAD_HANDLE.
  ResetSessionToken(element, leaf);

  fake_control.ReplyRemoveDependencies();
  RunLoopUntilIdle();

  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_error());
  EXPECT_EQ(suspend_result->error_value(), ZX_ERR_INTERNAL);

  // Leaf dependency must be rolled back (restored).
  EXPECT_TRUE(element.HasDependency(leaf));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(leaf));
  EXPECT_TRUE(fake_control.HasDependencyOn(leaf->DuplicatePowerToken()));
}

TEST_F(AllDriversElementTest, TestSuspendLeafAddsEligibleAncestorDependencyAndResumeRemovesIt) {
  auto parent = CreateNode("parent", root());
  auto child = CreateNode("child", parent);
  parent->UpdateBoundStateForTesting(true);
  child->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_TRUE(element.HasDependency(child));
  EXPECT_FALSE(element.HasDependency(parent));

  // Suspend the leaf node (child).
  zx::eventpair token;
  auto suspend_result = Suspend(element, "root.parent.child", &token);
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());

  // Child dependency is removed, and parent is added as a dependency because parent is eligible
  // (driver-bound, not stopping, has active power element) and has no other active descendants.
  EXPECT_EQ(fake_control.removed_dependencies_.size(), 1u);
  EXPECT_FALSE(element.HasDependency(child));
  EXPECT_TRUE(element.HasDependency(parent));
  EXPECT_EQ(fake_control.added_dependencies_.size(), 2u);

  // Resume child by dropping the suspend token.
  token.reset();
  RunLoopUntilIdle();

  // Child dependency is restored, and parent dependency is removed.
  EXPECT_TRUE(element.HasDependency(child));
  EXPECT_FALSE(element.HasDependency(parent));
  EXPECT_EQ(fake_control.removed_dependencies_.size(), 2u);
  EXPECT_EQ(fake_control.added_dependencies_.size(), 3u);
}

TEST_F(AllDriversElementTest,
       TestSuspendLeafWithRemainingActiveSiblingDoesNotAddAncestorDependency) {
  auto parent = CreateNode("parent", root());
  auto child_a = CreateNode("child_a", parent);
  auto child_b = CreateNode("child_b", parent);
  parent->UpdateBoundStateForTesting(true);
  child_a->UpdateBoundStateForTesting(true);
  child_b->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 2u);
  EXPECT_TRUE(element.HasDependency(child_a));
  EXPECT_TRUE(element.HasDependency(child_b));
  EXPECT_FALSE(element.HasDependency(parent));

  // Suspend child_a first.
  zx::eventpair token_a;
  auto suspend_a = Suspend(element, "root.parent.child_a", &token_a);
  ASSERT_TRUE(suspend_a.has_value());
  EXPECT_TRUE(suspend_a->is_ok());

  // child_a dependency removed, but parent is NOT added because child_b is still active.
  EXPECT_FALSE(element.HasDependency(child_a));
  EXPECT_TRUE(element.HasDependency(child_b));
  EXPECT_FALSE(element.HasDependency(parent));
  EXPECT_EQ(fake_control.added_dependencies_.size(), 2u);

  // Now suspend child_b as well.
  zx::eventpair token_b;
  auto suspend_b = Suspend(element, "root.parent.child_b", &token_b);
  ASSERT_TRUE(suspend_b.has_value());
  EXPECT_TRUE(suspend_b->is_ok());

  // child_b dependency removed, and now parent has no remaining active descendants, so parent is
  // added!
  EXPECT_FALSE(element.HasDependency(child_a));
  EXPECT_FALSE(element.HasDependency(child_b));
  EXPECT_TRUE(element.HasDependency(parent));
  EXPECT_EQ(fake_control.added_dependencies_.size(), 3u);

  // Resume child_a.
  token_a.reset();
  RunLoopUntilIdle();

  // child_a is added back, and parent dependency is removed!
  EXPECT_TRUE(element.HasDependency(child_a));
  EXPECT_FALSE(element.HasDependency(child_b));
  EXPECT_FALSE(element.HasDependency(parent));
}

TEST_F(AllDriversElementTest, TestSuspendLeafIneligibleAncestorTraversesToClosestEligible) {
  auto grandparent = CreateNode("grandparent", root());
  auto parent = CreateNode("parent", grandparent);
  auto child = CreateNode("child", parent);
  grandparent->UpdateBoundStateForTesting(true);
  // parent is unbound / not driver-bound, so parent is not eligible.
  parent->UpdateBoundStateForTesting(false);
  child->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_TRUE(element.HasDependency(child));
  EXPECT_FALSE(element.HasDependency(grandparent));

  // Suspend child.
  zx::eventpair token;
  auto suspend_result = Suspend(element, "root.grandparent.parent.child", &token);
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());

  // parent is skipped because it's not eligible; grandparent is eligible and added!
  EXPECT_FALSE(element.HasDependency(child));
  EXPECT_FALSE(element.HasDependency(parent));
  EXPECT_TRUE(element.HasDependency(grandparent));

  // Resume child.
  token.reset();
  RunLoopUntilIdle();

  // child restored, grandparent removed!
  EXPECT_TRUE(element.HasDependency(child));
  EXPECT_FALSE(element.HasDependency(grandparent));
}

TEST_F(AllDriversElementTest, TestSuspendLeafAddsAncestorDependencyBeforeRemovingLeaf) {
  auto parent = CreateNode("parent", root());
  auto child = CreateNode("child", parent);
  parent->UpdateBoundStateForTesting(true);
  child->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_TRUE(element.HasDependency(child));
  EXPECT_FALSE(element.HasDependency(parent));

  // Pause AddDependency replies so we can verify parent is added before child is removed.
  fake_control.auto_reply_add_dependency_ = false;

  zx::eventpair client_token, server_token;
  ASSERT_EQ(zx::eventpair::create(0, &client_token, &server_token), ZX_OK);
  std::optional<zx::result<>> suspend_result;
  element.StartSuspendSession(MakeTarget("root.parent.child"), std::move(server_token),
                              [&](zx::result<> res) { suspend_result = res; });
  RunLoopUntilIdle();

  // Ancestor AddDependency must be sent before child's RemoveDependency is called.
  EXPECT_FALSE(suspend_result.has_value());
  EXPECT_EQ(fake_control.added_dependencies_.size(), 2u);
  EXPECT_TRUE(element.HasDependency(parent));
  EXPECT_EQ(fake_control.removed_dependencies_.size(), 0u);
  EXPECT_TRUE(element.HasDependency(child));

  // Complete the ancestor AddDependency call; now child's RemoveDependency runs and completes.
  fake_control.ReplyAddDependencies();
  RunLoopUntilIdle();

  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());
  EXPECT_EQ(fake_control.removed_dependencies_.size(), 1u);
  EXPECT_FALSE(element.HasDependency(child));
  EXPECT_TRUE(element.HasDependency(parent));
}

TEST_F(AllDriversElementTest, TestSuspendLeafAncestorAddFailureDoesNotRemoveLeaf) {
  auto parent = CreateNode("parent", root());
  auto child = CreateNode("child", parent);
  parent->UpdateBoundStateForTesting(true);
  child->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_TRUE(element.HasDependency(child));

  fake_control.add_dependency_error_ = fuchsia_power_broker::ModifyDependencyError::kNotAuthorized;

  zx::eventpair token;
  auto suspend_result = Suspend(element, "root.parent.child", &token);
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_error());
  EXPECT_EQ(suspend_result->error_value(), ZX_ERR_ACCESS_DENIED);

  EXPECT_EQ(fake_control.removed_dependencies_.size(), 0u);
  EXPECT_TRUE(element.HasDependency(child));
  EXPECT_FALSE(element.HasDependency(parent));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(child));
}

TEST_F(AllDriversElementTest, TestSuspendLeafRemoveFailureRollsBackAddedAncestorDependency) {
  auto parent = CreateNode("parent", root());
  auto child = CreateNode("child", parent);
  parent->UpdateBoundStateForTesting(true);
  child->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_TRUE(element.HasDependency(child));

  fake_control.fail_remove_dependency_koid_ =
      FakeElementControl::GetKoid(child->DuplicatePowerToken());

  zx::eventpair token;
  auto suspend_result = Suspend(element, "root.parent.child", &token);
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_error());
  EXPECT_EQ(suspend_result->error_value(), ZX_ERR_ACCESS_DENIED);

  // Leaf dependency remains and added parent dependency was cleaned up on rollback.
  EXPECT_TRUE(element.HasDependency(child));
  EXPECT_FALSE(element.HasDependency(parent));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(child));
  EXPECT_FALSE(fake_control.HasDependencyOn(parent->DuplicatePowerToken()));
}

TEST_F(AllDriversElementTest,
       TestSuspendAddsAllEligibleAncestorsIncludingCompositeParentsBeforeRemovingLeaves) {
  auto grandparent = CreateNode("grandparent", root());
  auto target = CreateNode("target", grandparent);
  auto sibling_leaf = CreateNode("sibling_leaf", target);
  auto external_parent = CreateNode("external_parent", root());
  std::vector<std::weak_ptr<Node>> composite_parents = {target, external_parent};
  auto composite_leaf = CreateCompositeNode("composite_leaf", composite_parents, {{}, {}});

  for (const auto& node : {grandparent, target, sibling_leaf, external_parent, composite_leaf}) {
    node->UpdateBoundStateForTesting(true);
  }

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 2u);
  EXPECT_TRUE(element.HasDependency(sibling_leaf));
  EXPECT_TRUE(element.HasDependency(composite_leaf));
  EXPECT_FALSE(element.HasDependency(grandparent));
  EXPECT_FALSE(element.HasDependency(external_parent));

  // Pause AddDependency replies so we can verify both grandparent and external_parent are added
  // before either leaf dependency is removed.
  fake_control.auto_reply_add_dependency_ = false;

  zx::eventpair client_token, server_token;
  ASSERT_EQ(zx::eventpair::create(0, &client_token, &server_token), ZX_OK);
  std::optional<zx::result<>> suspend_result;
  element.StartSuspendSession(MakeTarget("root.grandparent.target"), std::move(server_token),
                              [&](zx::result<> res) { suspend_result = res; });
  RunLoopUntilIdle();

  // Both grandparent (continuing past suspended target) and external_parent (composite secondary
  // branch from composite_leaf) must have AddDependency sent before any RemoveDependency runs.
  EXPECT_FALSE(suspend_result.has_value());
  EXPECT_EQ(fake_control.added_dependencies_.size(), 4u);
  EXPECT_TRUE(element.HasDependency(grandparent));
  EXPECT_TRUE(element.HasDependency(external_parent));
  EXPECT_EQ(fake_control.removed_dependencies_.size(), 0u);
  EXPECT_TRUE(element.HasDependency(sibling_leaf));
  EXPECT_TRUE(element.HasDependency(composite_leaf));

  // Complete all pending AddDependency calls; now leaf RemoveDependency calls run and complete.
  fake_control.ReplyAddDependencies();
  RunLoopUntilIdle();

  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());
  EXPECT_EQ(fake_control.removed_dependencies_.size(), 2u);
  EXPECT_FALSE(element.HasDependency(sibling_leaf));
  EXPECT_FALSE(element.HasDependency(composite_leaf));
  EXPECT_TRUE(element.HasDependency(grandparent));
  EXPECT_TRUE(element.HasDependency(external_parent));
}

TEST_F(AllDriversElementTest,
       TestStartSuspendSessionUnbindWhileRemoveInFlightRestoresLeafDependency) {
  auto parent = CreateNode("parent", root());
  auto child = CreateNode("child", parent);
  parent->UpdateBoundStateForTesting(true);
  child->UpdateBoundStateForTesting(true);

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fake_control.auto_reply_remove_dependency_ = false;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_EQ(fake_control.added_dependencies_.size(), 1u);
  EXPECT_TRUE(element.HasDependency(child));

  // StartSuspendSession dispatches RemoveDependency for child and synchronously erases child from
  // dependencies_ so concurrent unbind/cancel observes that the dependency is no longer held.
  zx::eventpair client_token, server_token;
  ASSERT_EQ(zx::eventpair::create(0, &client_token, &server_token), ZX_OK);
  std::optional<zx::result<>> suspend_result;
  element.StartSuspendSession(MakeTarget("root.parent"), std::move(server_token),
                              [&](zx::result<> res) { suspend_result = res; });
  RunLoopUntilIdle();

  EXPECT_FALSE(suspend_result.has_value());
  EXPECT_EQ(fake_control.remove_dependency_completers_.size(), 1u);
  EXPECT_FALSE(element.HasDependency(child));

  // Unbind parent while RemoveDependency is in flight. CancelSuspendSession must re-add child.
  parent->UpdateBoundStateForTesting(false);
  fake_control.RemoveDependencyOn(parent->DuplicatePowerToken());
  element.OnNodeUnbinding(parent);
  RunLoopUntilIdle();

  EXPECT_EQ(fake_control.added_dependencies_.size(), 2u);
  EXPECT_TRUE(element.HasDependency(child));
  EXPECT_TRUE(fake_control.HasDependencyOn(child->DuplicatePowerToken()));

  // When the earlier in-flight RemoveDependency completes with ok, it must not erase the newly
  // restored dependency on child.
  fake_control.ReplyRemoveDependencies();
  RunLoopUntilIdle();

  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_error());
  EXPECT_EQ(suspend_result->error_value(), ZX_ERR_INTERNAL);
  EXPECT_TRUE(element.HasDependency(child));
  EXPECT_TRUE(fake_control.HasDependencyOn(child->DuplicatePowerToken()));
}

TEST_F(AllDriversElementTest, TestStartSuspendSessionDisambiguatesSanitizedMonikerCollision) {
  auto node_a = CreateNode("dev:0", root());
  auto node_b = CreateNode("dev.0", root());
  node_a->UpdateBoundStateForTesting(true);
  node_b->UpdateBoundStateForTesting(true);
  ASSERT_EQ(node_a->MakeComponentMoniker(), "root.dev_0");
  ASSERT_EQ(node_b->MakeComponentMoniker(), "root.dev_0");
  ASSERT_NE(node_a->MakeDisambiguator(), node_b->MakeDisambiguator());

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_TRUE(element.HasDependency(node_a));
  ASSERT_TRUE(element.HasDependency(node_b));

  // Moniker alone is ambiguous and returns ZX_ERR_INVALID_ARGS without changing dependencies.
  auto suspend_ambiguous = Suspend(element, "root.dev_0");
  ASSERT_TRUE(suspend_ambiguous.has_value());
  EXPECT_EQ(suspend_ambiguous->status_value(), ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(element.HasDependency(node_a));
  EXPECT_TRUE(element.HasDependency(node_b));

  // Providing node_a's disambiguator selects only node_a.
  zx::eventpair token_a;
  auto suspend_a = Suspend(element,
                           fuchsia_driver_development::NodeTarget{{
                               .moniker = "root.dev_0",
                               .disambiguator = node_a->MakeDisambiguator(),
                           }},
                           &token_a);
  ASSERT_TRUE(suspend_a.has_value());
  EXPECT_TRUE(suspend_a->is_ok());
  EXPECT_FALSE(element.HasDependency(node_a));
  EXPECT_TRUE(element.HasDependency(node_b));
}

TEST_F(AllDriversElementTest, TestStartSuspendSessionDisambiguatesByDisambiguator) {
  // Two nodes with identical moniker ("root.bus.dev") via consecutive duplicate segment
  // deduplication.
  auto bus = CreateNode("bus", root());
  auto bus_dup = CreateNode("bus", bus);
  auto dev_a = CreateNode("dev", bus);
  auto dev_b = CreateNode("dev", bus_dup);
  for (const auto& node : {bus, bus_dup, dev_a, dev_b}) {
    node->UpdateBoundStateForTesting(true);
  }
  ASSERT_EQ(dev_a->MakeComponentMoniker(), "root.bus.dev");
  ASSERT_EQ(dev_b->MakeComponentMoniker(), "root.bus.dev");
  ASSERT_NE(dev_a->MakeDisambiguator(), dev_b->MakeDisambiguator());

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();

  // Moniker alone still collides and returns ZX_ERR_INVALID_ARGS.
  auto suspend_ambiguous = Suspend(element, "root.bus.dev");
  ASSERT_TRUE(suspend_ambiguous.has_value());
  EXPECT_EQ(suspend_ambiguous->status_value(), ZX_ERR_INVALID_ARGS);

  // Providing dev_b's structural disambiguator selects only dev_b.
  zx::eventpair token_b;
  auto suspend_b = Suspend(element,
                           fuchsia_driver_development::NodeTarget{{
                               .moniker = "root.bus.dev",
                               .disambiguator = dev_b->MakeDisambiguator(),
                           }},
                           &token_b);
  ASSERT_TRUE(suspend_b.has_value());
  EXPECT_TRUE(suspend_b->is_ok());
  EXPECT_TRUE(element.HasDependency(dev_a));
  EXPECT_FALSE(element.HasDependency(dev_b));

  // Mismatched disambiguator returns ZX_ERR_NOT_FOUND.
  auto suspend_wrong = Suspend(element, fuchsia_driver_development::NodeTarget{{
                                            .moniker = "root.bus.dev",
                                            .disambiguator = "0000000000000000",
                                        }});
  ASSERT_TRUE(suspend_wrong.has_value());
  EXPECT_EQ(suspend_wrong->status_value(), ZX_ERR_NOT_FOUND);
}

TEST_F(AllDriversElementTest, TestStartSuspendSessionCompositeNodeDirectly) {
  auto parent_a = CreateNode("parent_a", root());
  auto parent_b = CreateNode("parent_b", root());
  std::vector<std::weak_ptr<Node>> parents = {parent_a, parent_b};
  auto composite = CreateCompositeNode("composite", parents, {{}, {}});

  for (const auto& node : {parent_a, parent_b, composite}) {
    node->UpdateBoundStateForTesting(true);
  }

  AllDriversElement& element = CreateElement();
  FakeElementControl fake_control;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> bindings;
  AttachFakeControl(element, fake_control, bindings);

  CompleteStartupTransition(element);
  RunLoopUntilIdle();
  ASSERT_TRUE(element.HasDependency(composite));
  ASSERT_FALSE(element.HasDependency(parent_a));
  ASSERT_FALSE(element.HasDependency(parent_b));

  // Targeting the multi-parent composite node by moniker must visit it once despite being
  // reachable via both parent_a and parent_b, rather than failing with ZX_ERR_INVALID_ARGS.
  auto suspend_result = Suspend(element, composite->MakeComponentMoniker());
  ASSERT_TRUE(suspend_result.has_value());
  EXPECT_TRUE(suspend_result->is_ok());
  EXPECT_FALSE(element.HasDependency(composite));
  EXPECT_TRUE(element.HasDependency(parent_a));
  EXPECT_TRUE(element.HasDependency(parent_b));
  EXPECT_TRUE(element.IsNodeOrAncestorSuspended(composite));

  Resume(composite->MakeComponentMoniker());
  EXPECT_TRUE(element.HasDependency(composite));
  EXPECT_FALSE(element.HasDependency(parent_a));
  EXPECT_FALSE(element.HasDependency(parent_b));
  EXPECT_FALSE(element.IsNodeOrAncestorSuspended(composite));
}

}  // namespace
}  // namespace driver_manager
