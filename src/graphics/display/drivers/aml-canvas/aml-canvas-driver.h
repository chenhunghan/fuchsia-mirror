// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_DRIVERS_AML_CANVAS_AML_CANVAS_DRIVER_H_
#define SRC_GRAPHICS_DISPLAY_DRIVERS_AML_CANVAS_AML_CANVAS_DRIVER_H_

#include <fidl/fuchsia.driver.framework/cpp/wire.h>
#include <lib/driver/compat/cpp/device_server.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/inspect/component/cpp/component.h>
#include <lib/inspect/cpp/inspector.h>
#include <lib/zx/result.h>

#include <memory>
#include <optional>
#include <vector>

#include "src/graphics/display/drivers/aml-canvas/aml-canvas.h"
#include "src/graphics/display/drivers/aml-canvas/aml-canvas_parser.h"

namespace aml_canvas {

// Driver instance that binds to the canvas board device.
//
// This class is responsible for interfacing with the Fuchsia Driver Framework.
class AmlCanvasDriver : public fdf::DriverBase2 {
 public:
  explicit AmlCanvasDriver();
  ~AmlCanvasDriver() override = default;

  AmlCanvasDriver(const AmlCanvasDriver&) = delete;
  AmlCanvasDriver(AmlCanvasDriver&&) = delete;
  AmlCanvasDriver& operator=(const AmlCanvasDriver&) = delete;
  AmlCanvasDriver& operator=(AmlCanvasDriver&&) = delete;

  // `fdf::DriverBase` methods.
  zx::result<> Start(fdf::DriverContext context) override;
  void Stop(fdf::StopCompleter completer) override;

 private:
  // Creates an AmlCanvas using the resources provided by the driver component
  // and adds the [`fuchsia.hardware.amlogiccanvas/Service`] it serves to the
  // driver component's outgoing directory.
  zx::result<std::unique_ptr<AmlCanvas>> CreateAndServeCanvas(const fdf::Namespace& incoming,
                                                              inspect::Inspector inspector);

  // Sets up the service offering of the child node, and adds the child node to
  // the node topology.
  //
  // If `compat_server` is not null, also configures the child node to provide
  // the node resources to descendant DFv1 drivers via the
  // [`fuchsia.driver.compat/Device`] interface using the provided
  // `compat_server`.
  //
  // On success, returns a client handle to the NodeController for the driver
  // to remove the node or manage the driver binding.
  zx::result<fidl::ClientEnd<fuchsia_driver_framework::NodeController>> AddChildNode(
      compat::SyncInitializedDeviceServer* compat_server);

  // Returns the canvas consumers from the metadata that DML boards generate, or std::nullopt if
  // the board doesn't provide it.
  static zx::result<std::optional<std::vector<aml_canvas_metadata::CanvasConsumer>>>
  GetDmlConsumers(const fdf::Namespace& incoming);

  // Adds a non-bindable node, named like the child node of `AddChildNode()`, with one child per
  // consumer. Each child offers the canvas service and carries the consumer's DML global ID as
  // `fuchsia.ID`, so that each consumer binds to its own child. The non-bindable node keeps the
  // node's moniker (e.g. `canvas-ff638000.aml-canvas`) the same as with `AddChildNode()`.
  //
  // On success, returns a client handle to the NodeController of the non-bindable node.
  zx::result<fidl::ClientEnd<fuchsia_driver_framework::NodeController>> AddConsumerNodes(
      const std::vector<aml_canvas_metadata::CanvasConsumer>& consumers);

  compat::SyncInitializedDeviceServer compat_server_;
  fidl::WireSyncClient<fuchsia_driver_framework::NodeController> controller_;

  // Only used with DML board metadata. See `AddConsumerNodes()`.
  fidl::ClientEnd<fuchsia_driver_framework::Node> consumers_node_;
  std::vector<fidl::ClientEnd<fuchsia_driver_framework::NodeController>> consumer_controllers_;

  std::unique_ptr<AmlCanvas> canvas_;
  inspect::Inspector inspector_;
  std::optional<inspect::ComponentInspector> component_inspector_;
};

}  // namespace aml_canvas

#endif  // SRC_GRAPHICS_DISPLAY_DRIVERS_AML_CANVAS_AML_CANVAS_DRIVER_H_
