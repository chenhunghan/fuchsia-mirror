// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BIN_DRIVER_MANAGER_POWER_ALL_DRIVERS_ELEMENT_H_
#define SRC_DEVICES_BIN_DRIVER_MANAGER_POWER_ALL_DRIVERS_ELEMENT_H_

#include <fidl/fuchsia.driver.development/cpp/fidl.h>
#include <fidl/fuchsia.power.broker/cpp/wire.h>
#include <lib/async/cpp/wait.h>
#include <lib/async/dispatcher.h>
#include <lib/zx/result.h>

#include <memory>
#include <optional>
#include <unordered_map>
#include <vector>

#include "src/devices/bin/driver_manager/node.h"

namespace driver_manager {

class PowerManager;

// AllDriversElement is an ElementRunner server that handles leasing all leaf driver nodes in
// the system.
class AllDriversElement : public fidl::WireServer<fuchsia_power_broker::ElementRunner>,
                          public std::enable_shared_from_this<AllDriversElement> {
 public:
  friend class AllDriversElementTest;

  // Tracks in-flight asynchronous operations when suspending the leaf nodes of a targeted node's
  // subtree.
  struct ModifyDependenciesState {
    // Number of in-flight dependency modification calls remaining to complete.
    size_t remaining;

    // Aggregated result of the operation; stores the first error encountered, if any.
    zx::result<> final_result = zx::ok();

    // Callback invoked once all dependency modifications have completed.
    fit::callback<void(zx::result<>)> cb;

    // The target node being suspended.
    std::weak_ptr<const Node> target_node;
  };

  AllDriversElement(PowerManager* power_manager, std::weak_ptr<Node> root,
                    zx::eventpair bootup_lease = {}, async_dispatcher_t* dispatcher = nullptr);

  void SetLevel(SetLevelRequestView request, SetLevelCompleter::Sync& completer) override;

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_power_broker::ElementRunner> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

  // Attaches the Power Broker element handles to this AllDriversElement, binding the
  // ElementRunner server endpoint to the dispatcher passed during construction.
  void AttachElement(PowerElementHandles handles);

  // Marks driver bootup complete and triggers the initial leaf-driver dependency sweep once the
  // element is at the active power level.
  void OnBootupComplete();

  // Called by DriverRunner whenever a new node is bound. If the node is a leaf driver, adds a
  // power dependency on the node to the AllDrivers element, passing the startup lease to be held
  // until the dependency addition completes.
  void OnNodeBound(std::shared_ptr<const Node> node);

  // Called by DriverRunner whenever a node is unbinding. If the node was the target of an active
  // suspend session, cancels the suspend session and restores power dependencies for any bound
  // leaf descendants in its subtree. If the node was a leaf driver, traverses up the topology to
  // find the closest bound ancestor(s) and adds a power dependency on any bound ancestor that is
  // now a leaf driver.
  void OnNodeUnbinding(std::shared_ptr<const Node> node);

  // Returns true if a power dependency on |node| has already been added to the AllDrivers element.
  bool HasDependency(const std::shared_ptr<const Node>& node) const {
    return dependencies_.contains(node.get());
  }

  // Returns true if |node| or any of its ancestors is currently suspended via
  // StartSuspendSession(). Used in OnNodeBound() to prevent re-adding power dependencies when a
  // descendant of a suspended driver restarts, and in OnNodeUnbinding() to avoid promoting a
  // suspended ancestor to a leaf driver.
  bool IsNodeOrAncestorSuspended(const std::shared_ptr<const Node>& node) const;

  // Suspends the driver node matching |target| and its dependents by removing their
  // dependencies from the AllDrivers element. Fails with ZX_ERR_ALREADY_EXISTS if a session is
  // already running for the driver, an ancestor, or all leaf drivers in its subtree. The driver
  // remains suspended until |token| is dropped or closed.
  void StartSuspendSession(const fuchsia_driver_development::NodeTarget& target,
                           zx::eventpair token, fit::callback<void(zx::result<>)> cb);

 private:
  // Traverses the topology bottom-up (post-order) to identify leaf driver nodes and adds
  // power dependencies on them, passing their startup leases to be dropped once the dependencies
  // are added. Should only be called once when the system first switches to the "on" level.
  void CompleteStartupTransition();

  // Processes startup transition logic for a single |node|:
  // If the node does not have a power element, does nothing.
  // Otherwise, retrieves the node's startup lease.
  //   - If |has_power_enabled_descendant| is false (the node is a power-enabled leaf driver
  //     instance), adds a power dependency on the node and passes the startup lease.
  //   - If |has_power_enabled_descendant| is true, the node is not a leaf; its startup lease is
  //     dropped.
  void ProcessStartupNode(const std::shared_ptr<const Node>& node,
                          bool has_power_enabled_descendant,
                          const std::shared_ptr<zx::eventpair>& bootup_lease);

  // Adds a power dependency on a leaf driver node to the AllDrivers element.
  // The optional |startup_lease| and |bootup_lease| are held until the AddDependency call completes
  // asynchronously, preventing the node and AllDrivers from dropping power before the dependency is
  // established. |bootup_lease| is only set during the startup transition.
  // Adding a dependency that already exists is harmless: Power Broker rejects it with
  // ALREADY_EXISTS, which is treated as success.
  // The optional |cb| is called with the result once the AddDependency call completes.
  void AddDependency(const std::shared_ptr<const Node>& node,
                     std::optional<zx::eventpair> startup_lease = std::nullopt,
                     fit::callback<void(zx::result<>)> cb = nullptr,
                     std::shared_ptr<zx::eventpair> bootup_lease = nullptr);

  // Removes a power dependency on a node from the AllDrivers element.
  // Callers must erase |node| from |dependencies_| synchronously before calling this method and
  // restore it in |cb| if the removal fails.
  // The optional |cb| is called with the result once the RemoveDependency call completes.
  void RemoveDependency(const std::shared_ptr<const Node>& node,
                        fit::callback<void(zx::result<>)> cb = nullptr);

  // Checks if any ancestors of |node| have a power dependency on the AllDrivers element,
  // and removes them because they are no longer leaf driver nodes.
  void RemoveAncestorDependencies(const std::shared_ptr<const Node>& node);

  // Traverses up the topology from |nodes| to find the closest eligible bound ancestor(s)
  // that have no other active descendants, and adds a power dependency on them to AllDrivers.
  // If |cb| is provided, invokes it with the aggregated result once all AddDependency calls
  // complete.
  void AddEligibleAncestorDependencies(const std::vector<std::shared_ptr<const Node>>& nodes,
                                       fit::callback<void(zx::result<>)> cb = nullptr);

  // Returns true if |node| is active with a power element, is not a hermetic power test,
  // and is not suspended under an active suspend session (directly or via an ancestor).
  bool IsEligibleForPowerDependency(const std::shared_ptr<const Node>& node) const;

  // Checks if |start_node| has any descendants that are eligible for a power dependency
  // (active with a power element, not a hermetic power test, and not suspended).
  bool HasEligiblePowerDependencyDescendants(const std::shared_ptr<const Node>& start_node) const;

  // Completes StartSuspendSession() once all RemoveDependency calls have finished. If any failed
  // or arming the session token wait fails, rolls back by re-adding any dependencies that were
  // removed from AllDrivers, deferring invocation of the caller's callback until all rollback
  // AddDependency calls complete.
  void CompleteSuspend(const std::shared_ptr<ModifyDependenciesState>& state);

  // Resumes the driver node at |target_node| and its dependents by adding back the dependencies
  // that StartSuspendSession() removed. Called automatically when a suspend session token is
  // dropped.
  void Resume(const std::shared_ptr<const Node>& target_node, fit::callback<void()> cb = nullptr);

  // Cancels an active suspend session targeting |node|, if any, closing its session token
  // and restoring power dependencies for any bound leaf descendants in its subtree.
  // Returns true if an active suspend session was found and cancelled.
  bool CancelSuspendSession(const std::shared_ptr<const Node>& node);

  // Result of collecting bound leaf driver nodes in the subtree of a target node.
  struct SubtreeLeafNodes {
    // The target node at the root of the suspend request subtree.
    std::weak_ptr<const Node> target_node;
    // Bound leaf driver nodes in the target node's subtree (or the target itself if it is a leaf).
    std::vector<std::weak_ptr<const Node>> leaf_nodes;
  };

  // Represents an active driver suspend session created by StartSuspendSession().
  // Retains a weak reference to the target node and watches the session token so that the driver
  // subtree is automatically resumed when the client end of the token is closed or dropped.
  struct SuspendSession {
    // The target node that was suspended.
    std::weak_ptr<const Node> target_node;
    // Server end of the suspend session token pair.
    zx::eventpair token;
    // Async wait listening for peer closure or signal on |token|.
    std::unique_ptr<async::WaitOnce> wait;
  };

  // Finds the target node matching |target| and collects all bound leaf driver nodes in its
  // subtree (or the target itself if it is a leaf). A leaf keeps all of its ancestors powered, so
  // only leaves have dependencies on AllDrivers.
  // Validates that the root node is reachable, the target node exists and is bound (and not
  // stopping), and that active leaf driver nodes exist in the subtree (returning ZX_ERR_BAD_STATE
  // if the target or a descendant leaf driver is unbound, stopping, or mid-restart).
  zx::result<SubtreeLeafNodes> GetSubtreeLeafNodes(
      const fuchsia_driver_development::NodeTarget& target) const;

  fidl::ServerBindingGroup<fuchsia_power_broker::ElementRunner> element_runner_bindings_;
  fidl::Client<fuchsia_power_broker::ElementControl> element_control_;
  fidl::Client<fuchsia_power_broker::Lessor> lessor_;

  // The current power level of this element. 0 represents low power, 1 represents the active/on
  // state.
  uint32_t current_level_ = 0;

  // Set to true once BootupTracker signals that initial driver binding has settled.
  bool bootup_complete_ = false;

  // Set to true when the element completes its initial leaf-driver startup transition.
  bool transition_complete_ = false;

  // Initial lease keeping AllDrivers at level 1 until OnBootupComplete() establishes all leaf
  // driver dependencies.
  zx::eventpair bootup_lease_;

  // A weak pointer to the root of the node topology, used to discover all nodes during the
  // startup transition.
  std::weak_ptr<Node> root_;

  // Owns this AllDriversElement.
  PowerManager* power_manager_;

  async_dispatcher_t* dispatcher_ = nullptr;

  // Tracks which nodes currently have a power dependency added to the AllDrivers element,
  // along with the number of in-flight/active dependency requests for each node.
  std::unordered_map<const Node*, size_t> dependencies_;

  // Tracks active suspend sessions keyed by target node.
  std::unordered_map<const Node*, std::unique_ptr<SuspendSession>> suspended_sessions_;
};

}  // namespace driver_manager

#endif  // SRC_DEVICES_BIN_DRIVER_MANAGER_POWER_ALL_DRIVERS_ELEMENT_H_
