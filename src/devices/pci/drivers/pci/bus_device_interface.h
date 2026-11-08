// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_DEVICES_PCI_DRIVERS_PCI_BUS_DEVICE_INTERFACE_H_
#define SRC_DEVICES_PCI_DRIVERS_PCI_BUS_DEVICE_INTERFACE_H_
#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.hardware.pci.root/cpp/wire.h>
#include <fidl/fuchsia.hardware.pci/cpp/fidl.h>
#include <lib/async/dispatcher.h>
#include <lib/zx/bti.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/msi.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>
#include <stdint.h>
#include <zircon/errors.h>

#include <optional>
#include <span>
#include <string_view>

#include <fbl/ref_ptr.h>

namespace pci {

// Forward declaration to avoid device.h
class Device;
// This interface allows for bridges/devices to communicate with the top level
// Bus object to add and remove themselves from the device list of their
// particular bus instance, obtain their BTIs, and make MSI allocations. This
// becomes more important as multiple bus instances with differing segment
// groups become a reality.
class BusDeviceInterface {
 public:
  virtual ~BusDeviceInterface() = default;
  // Get the BTI at |index| for a device.
  virtual zx_status_t GetBti(const pci::Device* device, uint32_t index, zx::bti* bti) = 0;
  virtual uint16_t GetSegmentGroup() = 0;
  // Allocate |count| messagge signaled interrupts for a device.
  virtual zx_status_t AllocateMsi(uint32_t count, zx::msi* out_allocation,
                                  fuchsia_hardware_pci_root::wire::MsiAllocationInfo* out_info) = 0;
  virtual zx_status_t GetMsiHandle(const zx::msi& allocation, uint32_t options, uint16_t msi_id,
                                   const zx::vmo& cfg_vmo, uint64_t cfg_offset,
                                   zx::interrupt* out_interrupt) = 0;
  // Add device to the Bus device tree.
  virtual zx_status_t LinkDevice(fbl::RefPtr<pci::Device> device) = 0;
  // Remove a device from the Bus device tree.
  virtual zx_status_t UnlinkDevice(pci::Device* device) = 0;
  // Shared IRQ handler list management for a device's legacy IRQ vector.
  // Note on lock hierarchy: Called while holding Device::dev_lock_; implementations
  // acquire Bus::devices_lock_. Note that Bus::HandleLegacyIrq acquires devices_lock_
  // before dev_lock_; concurrent deadlock is prevented because both operations execute
  // on the driver's synchronized dispatcher.
  virtual zx_status_t AddToSharedIrqList(pci::Device* device, uint32_t vector,
                                         zx::unowned_interrupt irq_handle) = 0;
  virtual zx_status_t RemoveFromSharedIrqList(pci::Device* device, uint32_t vector) = 0;

  // Publishes the PCI device's fuchsia.hardware.pci.Service instance, adds its child node,
  // and optionally registers its composite node spec.
  virtual zx::result<fidl::ClientEnd<fuchsia_driver_framework::NodeController>> AddDeviceNode(
      std::string_view name, fuchsia_hardware_pci::Service::InstanceHandler handler,
      std::span<const fuchsia_driver_framework::NodeProperty2> props,
      const fuchsia_driver_framework::BusInfo& bus_info,
      std::optional<fuchsia_driver_framework::CompositeNodeSpec> composite_spec) = 0;

  // Removes the PCI device's fuchsia.hardware.pci.Service instance when unplugged.
  virtual void RemoveDeviceService(std::string_view name) = 0;

  // Dispatcher on which per-device FIDL bindings are served.
  virtual async_dispatcher_t* binding_dispatcher() = 0;
};

}  // namespace pci

#endif  // SRC_DEVICES_PCI_DRIVERS_PCI_BUS_DEVICE_INTERFACE_H_
