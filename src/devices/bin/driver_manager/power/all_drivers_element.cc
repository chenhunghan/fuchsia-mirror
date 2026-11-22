// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/power/all_drivers_element.h"

#include <fidl/fuchsia.power.broker/cpp/fidl.h>
#include <zircon/assert.h>

#include <algorithm>
#include <deque>
#include <memory>
#include <string>
#include <string_view>
#include <unordered_set>
#include <vector>

#include "src/devices/bin/driver_manager/power/power_manager.h"
#include "src/devices/lib/log/log.h"

namespace driver_manager {

namespace {

// Performs a bottom-up, post-order Depth-First-Search (DFS) over the node topology
// starting from |starting_node|.
void PerformPostOrderDFS(const std::shared_ptr<const Node>& starting_node,
                         fit::function<void(const std::shared_ptr<const Node>&)> visitor) {
  std::unordered_set<const Node*> visited;
  std::vector<std::shared_ptr<const Node>> stack;

  stack.push_back(starting_node);
  visited.insert(starting_node.get());

  while (!stack.empty()) {
    auto curr = stack.back();

    bool pushed_child = false;
    for (const auto& child : curr->children()) {
      if (!visited.contains(child.get())) {
        visited.insert(child.get());
        stack.push_back(child);
        pushed_child = true;
        break;
      }
    }

    if (!pushed_child) {
      visitor(curr);
      stack.pop_back();
    }
  }
}

bool IsActiveWithPowerElement(const std::shared_ptr<const Node>& node) {
  return node->HasActivePowerElement() && !node->IsStoppingOrStopped();
}

// Performs the same traversal as PerformPostOrderDFS() and also tells |visitor| whether the visited
// node has any descendants with an active power element.
void PerformPostOrderDFSWithPowerEnabledDescendants(
    const std::shared_ptr<const Node>& starting_node,
    fit::function<void(const std::shared_ptr<const Node>&, bool has_power_enabled_descendant)>
        visitor) {
  // Memoization set to track which nodes have power-enabled descendants.
  // Bottom-up post-order DFS guarantees that when we visit a node, we have already processed
  // all of its children, allowing O(1) checks for power-enabled descendants.
  std::unordered_set<const Node*> has_power_enabled_descendant_set;

  PerformPostOrderDFS(starting_node, [&](const std::shared_ptr<const Node>& node) {
    // Check if this node has any descendants with an active power element.
    bool has_power_enabled_descendant =
        std::ranges::any_of(node->children(), [&](const auto& child) {
          return IsActiveWithPowerElement(child) ||
                 has_power_enabled_descendant_set.contains(child.get());
        });
    if (has_power_enabled_descendant) {
      has_power_enabled_descendant_set.insert(node.get());
    }

    visitor(node, has_power_enabled_descendant);
  });
}

// Collects all power-enabled leaf driver nodes in the subtree rooted at |target| (or |target|
// itself if it is a leaf). Hermetic power test nodes never depend on the AllDrivers element.
// Note: This function does not filter out suspended nodes:
// 1. Resume() needs the subtree's leaf nodes to re-add their power dependencies.
// 2. StartSuspendSession() filters out already-suspended nodes via HasDependency().
// 3. Treating suspended descendants as inactive would cause non-leaf ancestors to be falsely
//    identified as leaves.
std::vector<std::shared_ptr<const Node>> GetLeafDriverNodes(
    const std::shared_ptr<const Node>& target) {
  std::vector<std::shared_ptr<const Node>> leaf_nodes;
  PerformPostOrderDFSWithPowerEnabledDescendants(
      target,
      [&leaf_nodes](const std::shared_ptr<const Node>& node, bool has_power_enabled_descendant) {
        if (IsActiveWithPowerElement(node) && !has_power_enabled_descendant &&
            !node->IsHermeticPowerTest()) {
          leaf_nodes.push_back(node);
        }
      });
  return leaf_nodes;
}

// Checks whether |start_node| has any active bound descendants in its subtree.
// Performs an early-exit traversal without visiting the entire subtree or allocating a vector of
// leaves. Used for single-node checks in OnNodeBound() and OnNodeUnbinding().
// (In contrast, GetLeafDriverNodes() performs a full post-order traversal to collect all leaf
// nodes in a subtree for suspend/resume).
bool HasPowerEnabledDescendants(const std::shared_ptr<const Node>& start_node) {
  std::unordered_set<const Node*> visited;
  std::vector<std::shared_ptr<const Node>> stack;

  for (const auto& child : start_node->children()) {
    if (visited.insert(child.get()).second) {
      stack.push_back(child);
    }
  }

  while (!stack.empty()) {
    auto current = stack.back();
    stack.pop_back();

    if (IsActiveWithPowerElement(current)) {
      return true;
    }

    for (const auto& child : current->children()) {
      if (visited.insert(child.get()).second) {
        stack.push_back(child);
      }
    }
  }
  return false;
}

zx::result<std::shared_ptr<const Node>> FindNodeByTarget(
    const std::shared_ptr<const Node>& root, const fuchsia_driver_development::NodeTarget& target) {
  if (!target.moniker().has_value() || target.moniker()->empty()) {
    fdf_log::error("StartSuspendSession target is missing required moniker");
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  const std::string& target_moniker = *target.moniker();
  std::vector<std::shared_ptr<const Node>> matches;

  std::unordered_set<const Node*> visited;
  std::deque<std::shared_ptr<const Node>> queue;
  visited.insert(root.get());
  queue.push_back(root);

  while (!queue.empty()) {
    auto current = std::move(queue.front());
    queue.pop_front();

    if (current->MakeComponentMoniker() == target_moniker &&
        (!target.disambiguator().has_value() ||
         current->MakeDisambiguator() == *target.disambiguator())) {
      matches.push_back(current);
    }

    for (const auto& child : current->children()) {
      if (visited.insert(child.get()).second) {
        queue.push_back(child);
      }
    }
  }

  if (matches.empty()) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  if (matches.size() > 1) {
    fdf_log::error("Ambiguous suspend target for moniker '{}': matched {} nodes", target_moniker,
                   matches.size());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  return zx::ok(std::move(matches[0]));
}

// Converts the error of a failed Power Broker AddDependency or RemoveDependency call to a
// zx_status_t.
template <typename Error>
zx_status_t ModifyDependencyErrorToZxStatus(const Error& error) {
  if (error.is_framework_error()) {
    return ZX_ERR_INTERNAL;
  }
  switch (error.domain_error()) {
    case fuchsia_power_broker::ModifyDependencyError::kNotAuthorized:
      return ZX_ERR_ACCESS_DENIED;
    case fuchsia_power_broker::ModifyDependencyError::kNotFound:
      return ZX_ERR_NOT_FOUND;
    default:
      return ZX_ERR_INTERNAL;
  }
}

}  // namespace

AllDriversElement::AllDriversElement(PowerManager* power_manager, std::weak_ptr<Node> root,
                                     zx::eventpair bootup_lease, async_dispatcher_t* dispatcher)
    : bootup_lease_(std::move(bootup_lease)),
      root_(std::move(root)),
      power_manager_(power_manager),
      dispatcher_(dispatcher) {}

void AllDriversElement::AttachElement(PowerElementHandles handles) {
  element_control_ = std::move(handles.element_control);
  lessor_ = std::move(handles.lessor);
  if (handles.element_runner.is_valid()) {
    element_runner_bindings_.AddBinding(dispatcher_, std::move(handles.element_runner), this,
                                        fidl::kIgnoreBindingClosure);
  }
}

void AllDriversElement::OnBootupComplete() {
  bootup_complete_ = true;
  if (!transition_complete_) {
    CompleteStartupTransition();
  }
}

bool AllDriversElement::IsNodeOrAncestorSuspended(const std::shared_ptr<const Node>& node) const {
  if (suspended_sessions_.empty()) {
    return false;
  }

  // Check primary ancestors (including the node itself).
  for (const Node* cur = node.get(); cur != nullptr; cur = cur->GetPrimaryParent()) {
    if (suspended_sessions_.contains(cur)) {
      return true;
    }
  }

  // Topological paths are unique via primary parents, so the primary parent walk above
  // already covers all primary ancestors. We only need a DAG walk if there are composite
  // nodes with multiple parents in the ancestry.
  bool has_composite_ancestor = false;
  for (const Node* cur = node.get(); cur != nullptr; cur = cur->GetPrimaryParent()) {
    if (cur->parents().size() > 1) {
      has_composite_ancestor = true;
      break;
    }
  }
  if (!has_composite_ancestor) {
    return false;
  }

  // Check non-primary DAG ancestor branches (e.g. for composite nodes with multiple parents).
  std::unordered_set<const Node*> visited;
  std::vector<std::shared_ptr<const Node>> stack;

  // Mark all primary ancestors as visited since they were already verified not to be suspended.
  for (const Node* cur = node.get(); cur != nullptr; cur = cur->GetPrimaryParent()) {
    visited.insert(cur);
  }

  auto push_unvisited_parents = [&](const Node* n) {
    for (const auto& parent_weak : n->parents()) {
      if (auto parent = parent_weak.lock(); parent && visited.insert(parent.get()).second) {
        stack.push_back(std::move(parent));
      }
    }
  };

  // Seed the stack with non-primary parents from composite nodes along the primary chain.
  for (const Node* cur = node.get(); cur != nullptr; cur = cur->GetPrimaryParent()) {
    if (cur->parents().size() > 1) {
      push_unvisited_parents(cur);
    }
  }

  while (!stack.empty()) {
    auto current = stack.back();
    stack.pop_back();

    if (suspended_sessions_.contains(current.get())) {
      return true;
    }

    push_unvisited_parents(current.get());
  }

  return false;
}

void AllDriversElement::OnNodeBound(std::shared_ptr<const Node> node) {
  if (!transition_complete_ || !node->HasActivePowerElement()) {
    return;
  }
  // During normal operation, every power-enabled bound node takes its startup lease out. If
  // it is not a leaf node, this lease will immediately fall out of scope and be safely
  // dropped.
  std::optional<zx::eventpair> startup_lease =
      std::const_pointer_cast<Node>(node)->TakeStartupLease();

  if (node->IsHermeticPowerTest()) {
    return;
  }

  if (IsNodeOrAncestorSuspended(node)) {
    // If an ancestor is still suspended under another session, do not add a power dependency.
    // The startup lease goes out of scope and is dropped so the driver remains powered down.
    return;
  }

  if (HasPowerEnabledDescendants(node)) {
    // Already has power-enabled descendants, so it is not a leaf and we want to drop its
    // startup lease.
    return;
  }

  AddDependency(node, std::move(startup_lease));
}

void AllDriversElement::OnNodeUnbinding(std::shared_ptr<const Node> node) {
  if (!transition_complete_) {
    return;
  }

  if (node->IsHermeticPowerTest()) {
    return;
  }

  bool was_suspended_target = CancelSuspendSession(node);

  // If the node wasn't a leaf driver node that the AllDrivers element was
  // depending on or a suspended target node, return early.
  if (dependencies_.erase(node.get()) == 0 && !was_suspended_target) {
    return;
  }

  AddEligibleAncestorDependencies({node});
}

void AllDriversElement::AddDependency(const std::shared_ptr<const Node>& node,
                                      std::optional<zx::eventpair> startup_lease,
                                      fit::callback<void(zx::result<>)> cb,
                                      std::shared_ptr<zx::eventpair> bootup_lease) {
  zx::event power_token = node->DuplicatePowerToken();
  if (!power_token.is_valid()) {
    fdf_log::error(
        "Unable to duplicate power dependency token for node '{}' for All Drivers element",
        node->name());
    if (cb) {
      cb(zx::error(ZX_ERR_INTERNAL));
    }
    return;
  }

  dependencies_[node.get()]++;

  std::vector<fuchsia_power_broker::PowerLevel> requires_levels;
  requires_levels.push_back(1);
  fuchsia_power_broker::LevelDependency dep{{
      .dependent_level = 1,
      .requires_token = std::move(power_token),
      .requires_level_by_preference = std::move(requires_levels),
      .remove_with_required_element = true,
  }};

  element_control_->AddDependency(std::move(dep))
      .Then([weak_self = weak_from_this(), weak_node = std::weak_ptr<const Node>(node),
             name = node->name(), startup_lease = std::move(startup_lease),
             bootup_lease = std::move(bootup_lease), cb = std::move(cb)](
                fidl::Result<fuchsia_power_broker::ElementControl::AddDependency>& result) mutable {
        auto self = weak_self.lock();
        if (!self) {
          fdf_log::error("AllDriversElement destroyed while adding dependency for node '{}'", name);
          return;
        }
        auto node = weak_node.lock();
        zx::result<> status = zx::ok();
        if (result.is_error()) {
          // A node can be added twice. Node::is_bound() becomes true as soon as we ask the driver
          // host to start the driver, but OnNodeBound() is only called once that start completes.
          // If the startup transition runs in between, CompleteStartupTransition() adds the
          // dependency first and OnNodeBound() then adds it again. Power Broker rejects the
          // second request with ALREADY_EXISTS, which just means the dependency is already in
          // place.
          if (result.error_value().is_domain_error() &&
              result.error_value().domain_error() ==
                  fuchsia_power_broker::ModifyDependencyError::kAlreadyExists) {
            fdf_log::debug("Dependency on node '{}' already exists", name);
          } else {
            fdf_log::error("Failed to add dependency on node '{}': {}", name, result.error_value());
            status = zx::error(ModifyDependencyErrorToZxStatus(result.error_value()));
            if (node) {
              auto it = self->dependencies_.find(node.get());
              if (it != self->dependencies_.end()) {
                if (--it->second == 0) {
                  self->dependencies_.erase(it);
                }
              }
            }
          }
        }
        if (status.is_ok() && node) {
          self->RemoveAncestorDependencies(node);
        }
        if (cb) {
          cb(status);
        }
      });
}

void AllDriversElement::ProcessStartupNode(const std::shared_ptr<const Node>& node,
                                           bool has_power_enabled_descendant,
                                           const std::shared_ptr<zx::eventpair>& bootup_lease) {
  // We only process nodes that have a power element. Nodes without power elements do not take
  // startup leases or get added as dependencies of AllDrivers.
  if (!node->HasActivePowerElement()) {
    return;
  }

  // Retrieve the startup lease. It is crucial to take it here regardless of whether
  // this is a leaf or non-leaf node, so it doesn't get left behind.
  std::optional<zx::eventpair> startup_lease =
      std::const_pointer_cast<Node>(node)->TakeStartupLease();

  if (node->IsHermeticPowerTest()) {
    return;
  }

  // If this node has power-enabled descendants, it is not a leaf node in the active power
  // topology.
  if (has_power_enabled_descendant) {
    // The startup lease goes out of scope here and is dropped. It is safe to drop it
    // because leaf nodes (visited earlier in post-order DFS) already held their leases
    // synchronously, ensuring the parent node remains powered.
    return;
  }

  AddDependency(node, std::move(startup_lease), /*cb=*/nullptr, bootup_lease);
}

void AllDriversElement::CompleteStartupTransition() {
  if (current_level_ == 0) {
    return;
  }

  transition_complete_ = true;
  std::shared_ptr<zx::eventpair> bootup_lease;
  if (bootup_lease_.is_valid()) {
    bootup_lease = std::make_shared<zx::eventpair>(std::move(bootup_lease_));
  }

  // Identify all power-enabled leaf nodes and safely transfer their startup leases.
  if (auto root = root_.lock()) {
    PerformPostOrderDFSWithPowerEnabledDescendants(
        root, [this, &bootup_lease](const std::shared_ptr<const Node>& node,
                                    bool has_power_enabled_descendant) {
          ProcessStartupNode(node, has_power_enabled_descendant, bootup_lease);
        });
  }
}

void AllDriversElement::SetLevel(SetLevelRequestView request, SetLevelCompleter::Sync& completer) {
  current_level_ = request->level;

  if (bootup_complete_ && !transition_complete_) {
    CompleteStartupTransition();
  }

  // We can reply before the AddDependency() calls in CompleteStartupTransition() are complete
  // because the bootup lease and each leaf driver's startup lease will keep power elements on
  // until the dependencies are added.
  completer.Reply();
}

zx::result<AllDriversElement::SubtreeLeafNodes> AllDriversElement::GetSubtreeLeafNodes(
    const fuchsia_driver_development::NodeTarget& target) const {
  if (!transition_complete_) {
    return zx::error(ZX_ERR_UNAVAILABLE);
  }

  auto root = root_.lock();
  if (!root) {
    return zx::error(ZX_ERR_INTERNAL);
  }

  auto target_node_result = FindNodeByTarget(root, target);
  if (target_node_result.is_error()) {
    return target_node_result.take_error();
  }
  auto target_node = std::move(target_node_result.value());

  if (!target_node->is_bound() || target_node->IsStoppingOrStopped()) {
    return zx::error(ZX_ERR_BAD_STATE);
  }

  // If any descendant in the subtree is currently stopping or stopped, or is an unbound leaf
  // driver node that holds a power dependency (e.g. during a driver restart), report
  // ZX_ERR_BAD_STATE.
  bool has_unbound_or_stopping = false;
  PerformPostOrderDFS(target_node, [&](const std::shared_ptr<const Node>& node) {
    if (node->IsStoppingOrStopped() || (HasDependency(node) && !node->is_bound())) {
      has_unbound_or_stopping = true;
    }
  });
  if (has_unbound_or_stopping) {
    return zx::error(ZX_ERR_BAD_STATE);
  }

  // Collect all bound leaf driver nodes in the subtree rooted at the target (or the target itself
  // if it is a leaf). A leaf keeps all of its ancestors powered, so only leaves have dependencies
  // on AllDrivers to remove or re-add.
  std::vector<std::shared_ptr<const Node>> leaf_nodes = GetLeafDriverNodes(target_node);

  if (leaf_nodes.empty()) {
    fdf_log::error("No leaf driver nodes found for driver at node '{}'",
                   target_node->MakeComponentMoniker());
    if (target_node->IsHermeticPowerTest()) {
      return zx::error(ZX_ERR_NOT_SUPPORTED);
    }
    return zx::error(ZX_ERR_INTERNAL);
  }

  std::vector<std::weak_ptr<const Node>> weak_leaf_nodes;
  weak_leaf_nodes.reserve(leaf_nodes.size());
  for (const auto& leaf : leaf_nodes) {
    weak_leaf_nodes.push_back(leaf);
  }

  return zx::ok(SubtreeLeafNodes{
      .target_node = std::move(target_node),
      .leaf_nodes = std::move(weak_leaf_nodes),
  });
}

void AllDriversElement::StartSuspendSession(const fuchsia_driver_development::NodeTarget& target,
                                            zx::eventpair token,
                                            fit::callback<void(zx::result<>)> cb) {
  std::string_view moniker =
      target.moniker().has_value() ? std::string_view(*target.moniker()) : "<missing>";
  if (!token.is_valid()) {
    fdf_log::error("Invalid suspend token provided for driver at node '{}'", moniker);
    cb(zx::error(ZX_ERR_BAD_HANDLE));
    return;
  }

  auto nodes_result = GetSubtreeLeafNodes(target);
  if (nodes_result.is_error()) {
    cb(nodes_result.take_error());
    return;
  }
  auto target_node = nodes_result.value().target_node.lock();
  if (!target_node) {
    fdf_log::error("Target node '{}' was destroyed during GetSubtreeLeafNodes", moniker);
    cb(zx::error(ZX_ERR_INTERNAL));
    return;
  }
  const auto& leaf_nodes = nodes_result.value().leaf_nodes;

  // If the driver or an ancestor is already suspended, reject the request as
  // ZX_ERR_ALREADY_EXISTS.
  if (suspended_sessions_.contains(target_node.get()) || IsNodeOrAncestorSuspended(target_node)) {
    fdf_log::error("Cannot suspend driver at node '{}': suspend session already exists", moniker);
    cb(zx::error(ZX_ERR_ALREADY_EXISTS));
    return;
  }

  // Filter to only those leaf nodes that currently have a dependency added to AllDrivers.
  // If some descendant nodes in the subtree were already suspended, skipping them avoids Power
  // Broker NOT_FOUND errors.
  std::vector<std::shared_ptr<const Node>> active_leaves;
  for (const auto& weak_node : leaf_nodes) {
    if (auto node = weak_node.lock(); node && HasDependency(node)) {
      active_leaves.push_back(std::move(node));
    }
  }

  if (active_leaves.empty()) {
    fdf_log::error(
        "Cannot suspend driver at node '{}': all leaf drivers in subtree are already suspended",
        moniker);
    cb(zx::error(ZX_ERR_ALREADY_EXISTS));
    return;
  }

  // Mark the node as suspended immediately with its session so that:
  // 1. AddEligibleAncestorDependencies() recognizes the subtree as suspended when checking
  //    whether ancestors still have other active, non-suspended descendants.
  // 2. If a node in the target subtree binds while dependency modifications are in flight,
  //    OnNodeBound() drops the startup lease instead of re-adding a dependency.
  auto session = std::make_unique<SuspendSession>();
  session->target_node = target_node;
  session->token = std::move(token);
  suspended_sessions_[target_node.get()] = std::move(session);

  std::vector<std::weak_ptr<const Node>> weak_active_leaves;
  weak_active_leaves.reserve(active_leaves.size());
  for (const auto& leaf : active_leaves) {
    weak_active_leaves.push_back(leaf);
  }

  auto shared_cb = std::make_shared<fit::callback<void(zx::result<>)>>(std::move(cb));

  AddEligibleAncestorDependencies(
      active_leaves,
      [weak_self = weak_from_this(), weak_target = std::weak_ptr<const Node>(target_node),
       weak_active_leaves = std::move(weak_active_leaves), shared_cb](zx::result<> add_result) {
        auto self = weak_self.lock();
        if (!self) {
          if (auto target = weak_target.lock()) {
            fdf_log::error(
                "AllDriversElement destroyed while adding ancestor dependencies for '{}'",
                target->name());
          } else {
            fdf_log::error("AllDriversElement destroyed while adding ancestor dependencies");
          }
          return;
        }

        auto target = weak_target.lock();
        if (!target || !self->suspended_sessions_.contains(target.get())) {
          (*shared_cb)(zx::error(ZX_ERR_INTERNAL));
          return;
        }

        if (add_result.is_error()) {
          self->suspended_sessions_.erase(target.get());
          for (const auto& weak_leaf : weak_active_leaves) {
            if (auto leaf = weak_leaf.lock(); leaf && self->HasDependency(leaf)) {
              self->RemoveAncestorDependencies(leaf);
            }
          }
          (*shared_cb)(add_result);
          return;
        }

        std::vector<std::shared_ptr<const Node>> leaves_to_remove;
        for (const auto& weak_leaf : weak_active_leaves) {
          if (auto leaf = weak_leaf.lock(); leaf && self->HasDependency(leaf)) {
            leaves_to_remove.push_back(std::move(leaf));
          }
        }

        auto state = std::make_shared<ModifyDependenciesState>(ModifyDependenciesState{
            .remaining = leaves_to_remove.size(),
            .cb = std::move(*shared_cb),
            .target_node = target,
        });

        if (leaves_to_remove.empty()) {
          self->CompleteSuspend(state);
          return;
        }

        for (const auto& node : leaves_to_remove) {
          self->dependencies_.erase(node.get());
          self->RemoveDependency(node, [weak_self = self->weak_from_this(),
                                        weak_node = std::weak_ptr<const Node>(node),
                                        state](zx::result<> result) {
            auto self = weak_self.lock();
            if (!self) {
              if (auto target = state->target_node.lock()) {
                fdf_log::error("AllDriversElement destroyed while suspending driver at node '{}'",
                               target->name());
              } else {
                fdf_log::error("AllDriversElement destroyed while suspending driver");
              }
              return;
            }
            if (result.is_error()) {
              if (auto node = weak_node.lock()) {
                self->dependencies_[node.get()]++;
              }
              if (state->final_result.is_ok()) {
                state->final_result = result;
              }
            }
            if (--state->remaining == 0) {
              self->CompleteSuspend(state);
            }
          });
        }
      });
}

void AllDriversElement::RemoveDependency(const std::shared_ptr<const Node>& node,
                                         fit::callback<void(zx::result<>)> cb) {
  zx::event power_token = node->DuplicatePowerToken();
  if (!power_token.is_valid()) {
    fdf_log::error(
        "Unable to duplicate power dependency token for node '{}' for All Drivers element",
        node->name());
    if (cb) {
      cb(zx::error(ZX_ERR_INTERNAL));
    }
    return;
  }

  std::vector<fuchsia_power_broker::PowerLevel> requires_levels;
  requires_levels.push_back(1);
  fuchsia_power_broker::LevelDependency dep{{
      .dependent_level = 1,
      .requires_token = std::move(power_token),
      .requires_level_by_preference = std::move(requires_levels),
      .remove_with_required_element = true,
  }};

  element_control_->RemoveDependency(std::move(dep))
      .Then([weak_self = weak_from_this(), name = node->name(), cb = std::move(cb)](
                fidl::Result<fuchsia_power_broker::ElementControl::RemoveDependency>&
                    result) mutable {
        auto self = weak_self.lock();
        if (!self) {
          fdf_log::error("AllDriversElement destroyed while removing dependency for node '{}'",
                         name);
          return;
        }
        zx::result<> status = zx::ok();
        if (result.is_error()) {
          fdf_log::error("Failed to remove dependency on node '{}': {}", name,
                         result.error_value());
          status = zx::error(ModifyDependencyErrorToZxStatus(result.error_value()));
        }
        if (cb) {
          cb(status);
        }
      });
}

void AllDriversElement::RemoveAncestorDependencies(const std::shared_ptr<const Node>& node) {
  std::unordered_set<const Node*> visited;
  std::vector<std::shared_ptr<const Node>> stack;

  for (const auto& parent_weak : node->parents()) {
    if (auto parent = parent_weak.lock()) {
      if (visited.insert(parent.get()).second) {
        stack.push_back(std::move(parent));
      }
    }
  }

  while (!stack.empty()) {
    auto current = stack.back();
    stack.pop_back();

    if (dependencies_.erase(current.get()) > 0) {
      RemoveDependency(current, [weak_self = weak_from_this(),
                                 current_ptr = current.get()](zx::result<> result) {
        if (result.is_error()) {
          if (auto self = weak_self.lock()) {
            self->dependencies_[current_ptr]++;
          }
        }
      });
    }

    for (const auto& parent_weak : current->parents()) {
      if (auto parent = parent_weak.lock()) {
        if (visited.insert(parent.get()).second) {
          stack.push_back(std::move(parent));
        }
      }
    }
  }
}

bool AllDriversElement::IsEligibleForPowerDependency(
    const std::shared_ptr<const Node>& node) const {
  return IsActiveWithPowerElement(node) && !node->IsHermeticPowerTest() &&
         !IsNodeOrAncestorSuspended(node);
}

bool AllDriversElement::HasEligiblePowerDependencyDescendants(
    const std::shared_ptr<const Node>& start_node) const {
  std::unordered_set<const Node*> visited;
  std::vector<std::shared_ptr<const Node>> stack;

  for (const auto& child : start_node->children()) {
    if (visited.insert(child.get()).second) {
      stack.push_back(child);
    }
  }

  while (!stack.empty()) {
    auto current = stack.back();
    stack.pop_back();

    if (IsEligibleForPowerDependency(current)) {
      return true;
    }

    for (const auto& child : current->children()) {
      if (visited.insert(child.get()).second) {
        stack.push_back(child);
      }
    }
  }
  return false;
}

void AllDriversElement::AddEligibleAncestorDependencies(
    const std::vector<std::shared_ptr<const Node>>& nodes, fit::callback<void(zx::result<>)> cb) {
  std::unordered_set<const Node*> visited;
  std::vector<std::shared_ptr<const Node>> stack;
  std::vector<std::shared_ptr<const Node>> ancestors_to_add;

  for (const auto& node : nodes) {
    visited.insert(node.get());
  }
  for (const auto& node : nodes) {
    for (const auto& parent_weak : node->parents()) {
      if (auto parent = parent_weak.lock()) {
        if (visited.insert(parent.get()).second) {
          stack.push_back(std::move(parent));
        }
      }
    }
  }

  while (!stack.empty()) {
    auto current = stack.back();
    stack.pop_back();

    // Only consider active, non-suspended ancestors with a power element; skip unbound,
    // non-power-enabled, or suspended intermediates and continue traversing up their parent
    // branches.
    if (IsActiveWithPowerElement(current) && !IsNodeOrAncestorSuspended(current)) {
      // Promote |current| to an AllDrivers dependency only if it is eligible (not a hermetic power
      // test), is not already a dependency, and has no remaining active, non-suspended
      // power-enabled descendants keeping it powered.
      if (IsEligibleForPowerDependency(current) && !HasDependency(current) &&
          !HasEligiblePowerDependencyDescendants(current)) {
        ancestors_to_add.push_back(current);
      }
      // Stop traversing up this branch because |current| is bound and not suspended, so it either
      // was just added as a dependency or already has other active descendants keeping it and its
      // ancestors powered.
      continue;
    }

    for (const auto& parent_weak : current->parents()) {
      if (auto parent = parent_weak.lock()) {
        if (visited.insert(parent.get()).second) {
          stack.push_back(std::move(parent));
        }
      }
    }
  }

  if (ancestors_to_add.empty()) {
    if (cb) {
      cb(zx::ok());
    }
    return;
  }

  if (!cb) {
    for (const auto& ancestor : ancestors_to_add) {
      AddDependency(ancestor);
    }
    return;
  }

  auto state = std::make_shared<ModifyDependenciesState>(ModifyDependenciesState{
      .remaining = ancestors_to_add.size(),
      .cb = std::move(cb),
  });

  for (const auto& ancestor : ancestors_to_add) {
    AddDependency(ancestor, std::nullopt, [state](zx::result<> result) {
      if (result.is_error() && state->final_result.is_ok()) {
        state->final_result = result;
      }
      if (--state->remaining == 0) {
        state->cb(state->final_result);
      }
    });
  }
}

void AllDriversElement::CompleteSuspend(const std::shared_ptr<ModifyDependenciesState>& state) {
  auto target_node = state->target_node.lock();
  if (!target_node) {
    fdf_log::error("Target node destroyed during CompleteSuspend");
    state->cb(zx::error(ZX_ERR_INTERNAL));
    return;
  }

  // If all RemoveDependency calls succeeded, arm the async wait on the session token and complete.
  if (state->final_result.is_ok()) {
    if (auto it = suspended_sessions_.find(target_node.get()); it != suspended_sessions_.end()) {
      auto& session = it->second;
      session->wait = std::make_unique<async::WaitOnce>(
          session->token.get(), ZX_EVENTPAIR_PEER_CLOSED | ZX_EVENTPAIR_SIGNALED);
      async::WaitOnce* wait_ptr = session->wait.get();

      zx_status_t status = wait_ptr->Begin(
          dispatcher_, [weak_self = weak_from_this(), weak_target_node = state->target_node](
                           async_dispatcher_t* dispatcher, async::WaitOnce* wait,
                           zx_status_t status, const zx_packet_signal_t* signal) {
            // If the wait completed with an error status (typically ZX_ERR_CANCELED), do not
            // resume the driver subtree. ZX_ERR_CANCELED occurs when:
            // 1. Driver Manager or the async dispatcher is shutting down / being destroyed.
            //    Initiating new asynchronous Power Broker FIDL calls during shutdown can hang
            //    or interfere with clean system teardown, and power elements will be torn down
            //    regardless.
            // 2. The node is unbinding or being stopped while suspended (e.g. in
            //    OnNodeUnbinding()), which erased the session and destroyed the wait.
            //    Attempting to resume here would improperly re-add power dependencies
            //    to an unbinding driver.
            if (status != ZX_OK) {
              if (auto target = weak_target_node.lock()) {
                fdf_log::error("Wait on suspend token failed for node '{}': {}", target->name(),
                               zx_status_get_string(status));
              }
              return;
            }
            auto self = weak_self.lock();
            if (!self) {
              if (auto target = weak_target_node.lock()) {
                fdf_log::error(
                    "AllDriversElement destroyed while waiting on suspend token for node '{}'",
                    target->name());
              }
              return;
            }
            auto target = weak_target_node.lock();
            if (!target) {
              return;
            }
            fdf_log::info("Suspend token dropped for node '{}'; resuming driver subtree",
                          target->name());
            self->Resume(target);
          });
      if (status == ZX_OK) {
        state->cb(zx::ok());
        return;
      }

      fdf_log::error("Failed to begin async wait on suspend token for node '{}': {}",
                     target_node->name(), zx_status_get_string(status));
      session->wait.reset();
      state->final_result = zx::error(ZX_ERR_INTERNAL);
    } else {
      fdf_log::error("Suspend session not found for node '{}' during CompleteSuspend",
                     target_node->name());
      state->final_result = zx::error(ZX_ERR_INTERNAL);
    }
  }

  if (!suspended_sessions_.contains(target_node.get())) {
    state->cb(state->final_result);
    return;
  }

  // If StartSuspendSession failed, roll back by calling Resume(). This restores dependencies for
  // nodes removed from AllDrivers and also establishes dependencies for any nodes that bound
  // in the subtree while the suspend was in flight.
  Resume(target_node,
         [cb = std::move(state->cb), result = state->final_result]() mutable { cb(result); });
}

void AllDriversElement::Resume(const std::shared_ptr<const Node>& target_node,
                               fit::callback<void()> cb) {
  ZX_ASSERT_MSG(suspended_sessions_.contains(target_node.get()),
                "Resume() called for node '%s' which has no active suspend session",
                target_node->name().c_str());

  // Remove the node from |suspended_sessions_| immediately so that if a node binds while
  // Resume is in flight, OnNodeBound() will not see the ancestor as suspended and will
  // correctly establish its dependency. Since AddDependency() is idempotent, concurrent
  // binds are safe.
  suspended_sessions_.erase(target_node.get());

  if (!target_node->is_bound() || target_node->IsStoppingOrStopped()) {
    if (cb) {
      cb();
    }
    return;
  }

  // Collect leaf driver nodes in the subtree.
  std::vector<std::shared_ptr<const Node>> leaf_nodes = GetLeafDriverNodes(target_node);
  std::vector<std::shared_ptr<const Node>> leaves_to_resume;
  for (const auto& node : leaf_nodes) {
    if (!IsEligibleForPowerDependency(node)) {
      continue;
    }
    // Only add dependency if node does not already have one; otherwise ensure any eligible
    // ancestor dependencies added prior to RemoveDependency() are cleaned up (e.g. rollback).
    if (!HasDependency(node)) {
      leaves_to_resume.push_back(node);
    } else {
      RemoveAncestorDependencies(node);
    }
  }

  // It is possible for leaves_to_resume to be empty if:
  // 1. An ancestor node is still suspended under another session, its leaves remain
  //    suspended (excluded by IsNodeOrAncestorSuspended()) and will be resumed when the ancestor
  //    resumes.
  // 2. Or all descendant leaf nodes were unbound while suspended.
  if (leaves_to_resume.empty()) {
    if (cb) {
      cb();
    }
    return;
  }

  auto shared_cb = std::make_shared<fit::callback<void()>>(std::move(cb));
  auto remaining = std::make_shared<size_t>(leaves_to_resume.size());
  for (const auto& node : leaves_to_resume) {
    AddDependency(node, std::nullopt, [remaining, shared_cb](auto /*result*/) {
      if (--(*remaining) == 0 && *shared_cb) {
        (*shared_cb)();
      }
    });
  }
}

bool AllDriversElement::CancelSuspendSession(const std::shared_ptr<const Node>& node) {
  auto it = suspended_sessions_.find(node.get());
  if (it == suspended_sessions_.end()) {
    return false;
  }
  fdf_log::info("Target node '{}' unbinding; cancelling suspend session", node->name());
  suspended_sessions_.erase(it);

  std::vector<std::shared_ptr<const Node>> leaf_nodes = GetLeafDriverNodes(node);
  for (const auto& leaf : leaf_nodes) {
    if (leaf.get() != node.get() && !HasDependency(leaf) && IsEligibleForPowerDependency(leaf)) {
      AddDependency(leaf);
    }
  }
  return true;
}

void AllDriversElement::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_power_broker::ElementRunner> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  fdf_log::info("Unknown AllDrivers element runner method request received: {}",
                metadata.method_ordinal);
}

}  // namespace driver_manager
