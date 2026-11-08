// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BIN_DRIVER_MANAGER_POWER_ALL_DRIVERS_ELEMENT_H_
#define SRC_DEVICES_BIN_DRIVER_MANAGER_POWER_ALL_DRIVERS_ELEMENT_H_

#include <fidl/fuchsia.power.broker/cpp/wire.h>
#include <lib/async/dispatcher.h>
#include <lib/fit/defer.h>

#include <memory>
#include <optional>
#include <unordered_set>

#include "src/devices/bin/driver_manager/node.h"

namespace driver_manager {

class PowerManager;

// AllDriversElement is an ElementRunner server that handles leasing all leaf driver nodes in
// the system.
class AllDriversElement : public fidl::WireServer<fuchsia_power_broker::ElementRunner>,
                          public std::enable_shared_from_this<AllDriversElement> {
 public:
  friend class AllDriversElementTest;

  AllDriversElement(PowerManager* power_manager, std::shared_ptr<Node> root,
                    zx::eventpair bootup_lease = {});

  void SetLevel(SetLevelRequestView request, SetLevelCompleter::Sync& completer) override;

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_power_broker::ElementRunner> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

  void AttachElement(async_dispatcher_t* dispatcher, PowerElementHandles handles);

  // Marks driver bootup complete and triggers the initial leaf-driver dependency sweep once the
  // element is at the active power level.
  void OnBootupComplete();

  // Called by DriverRunner whenever a new node is bound. If the node is a leaf driver, adds a
  // power dependency on the node to the AllDrivers element, passing the startup lease to be held
  // until the dependency addition completes.
  void OnNodeBound(std::shared_ptr<const Node> node);

  // Called by DriverRunner whenever a node is unbinding. If the node was a leaf driver, traverses
  // up the topology to find the closest bound ancestor(s) and adds a power dependency on any
  // bound ancestor that is now a leaf driver.
  void OnNodeUnbinding(std::shared_ptr<const Node> node);

  // Returns true if a power dependency on |node| has already been added to the AllDrivers element.
  bool HasDependency(const std::shared_ptr<const Node>& node) const {
    return dependencies_.contains(node.get());
  }

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
  // ALREADY_EXISTS, which is ignored.
  void AddDependency(const std::shared_ptr<const Node>& node,
                     std::optional<zx::eventpair> startup_lease = std::nullopt,
                     std::shared_ptr<zx::eventpair> bootup_lease = nullptr);

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

  // Tracks which nodes currently have a power dependency added to the AllDrivers element.
  std::unordered_set<const Node*> dependencies_;
};

}  // namespace driver_manager

#endif  // SRC_DEVICES_BIN_DRIVER_MANAGER_POWER_ALL_DRIVERS_ELEMENT_H_
