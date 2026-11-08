// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/pci/drivers/pci/bus.h"

#include <fidl/fuchsia.hardware.pci.root/cpp/fidl.h>
#include <fidl/fuchsia.hardware.pci.root/cpp/wire.h>
#include <fidl/fuchsia.hardware.pci/cpp/natural_types.h>
#include <lib/driver/component/cpp/node_offers.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/metadata/cpp/metadata.h>
#include <lib/driver/mmio/cpp/mmio.h>
#include <lib/fit/defer.h>
#include <lib/pci/constants.h>
#include <lib/pci/hw.h>
#include <lib/stdcompat/span.h>
#include <lib/zx/time.h>
#include <zircon/errors.h>
#include <zircon/status.h>
#include <zircon/syscalls/port.h>

#include <algorithm>
#include <list>
#include <set>

#include <fbl/alloc_checker.h>
#include <fbl/array.h>
#include <fbl/auto_lock.h>
#include <fbl/vector.h>

#include "src/devices/pci/drivers/pci/bridge.h"
#include "src/devices/pci/drivers/pci/config.h"
#include "src/devices/pci/drivers/pci/device.h"

namespace pci {

zx::result<> Bus::Start(fdf::DriverContext context) {
  dispatcher_ = dispatcher();

  zx::result root_client_end =
      context.incoming().Connect<fuchsia_hardware_pci_root::RootService::Root>();
  if (root_client_end.is_error()) {
    fdf::error("failed to connect to RootService: {}", root_client_end);
    return root_client_end.take_error();
  }
  root_client_ = fidl::WireSyncClient<fuchsia_hardware_pci_root::Root>(std::move(*root_client_end));

  auto info_result = root_client_->GetPlatformInfo();
  if (!info_result.ok()) {
    fdf::error("failed to send GetPlatformInfo request: {}", info_result.status_string());
    return zx::error(info_result.status());
  }
  if (info_result->is_error()) {
    fdf::error("failed to obtain platform information from Root: {}",
               zx_status_get_string(info_result->error_value()));
    return zx::error(info_result->error_value());
  }

  info_ = fidl::ToNatural(info_result->value()->info);

  auto cam_result = root_client_->GetCam();
  if (!cam_result.ok()) {
    fdf::error("failed to send GetCam request: {}", cam_result.status_string());
    return zx::error(cam_result.status());
  }

  if (cam_result->is_ok()) {
    zx::result mmio_result = pci::Bus::MapConfigRegion(std::move(cam_result->value()->vmo));
    if (mmio_result.is_error()) {
      fdf::error("failed to map cam vmo: {}", mmio_result);
      return mmio_result.take_error();
    }
    ecam_ = std::move(*mmio_result);
    is_extended_ = cam_result->value()->is_extended;
  } else if (cam_result->error_value() == ZX_ERR_NOT_SUPPORTED) {
    zx::result config_client_end =
        context.incoming().Connect<fuchsia_hardware_pci_root::ConfigService::Config>();
    if (config_client_end.is_error()) {
      fdf::error("failed to connect to ConfigService: {}", config_client_end);
      return config_client_end.take_error();
    }
    config_client_.Bind(std::move(*config_client_end));
  } else {
    fdf::error("failed to get cam from Root: {}", zx_status_get_string(cam_result->error_value()));
    return zx::error(cam_result->error_value());
  }

  zx::result composite_manager =
      context.incoming().Connect<fuchsia_driver_framework::CompositeNodeManager>();
  if (composite_manager.is_error()) {
    fdf::error("failed to connect to CompositeNodeManager: {}", composite_manager);
    return composite_manager.take_error();
  }
  composite_manager_ = fidl::SyncClient(std::move(*composite_manager));

  if (zx::result result =
          fdf_metadata::GetMetadataIfExists<PciFidl::BoardConfiguration>(context.incoming());
      result.is_ok()) {
    if (std::optional config = std::move(result.value()); config.has_value()) {
      board_config_ = std::move(*config);
    }
  }

  zx_status_t status = Initialize();
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok();
}

zx_status_t Bus::Initialize() {
  zx::result result = outgoing()->AddService<fuchsia_hardware_pci::DebugService>(
      fuchsia_hardware_pci::DebugService::InstanceHandler({
          .debug = bindings_.CreateHandler(this, dispatcher_, fidl::kIgnoreBindingClosure),
      }));
  if (result.is_error()) {
    fdf::error("failed to add DebugService: {}", result);
    return result.status_value();
  }

  zx::result bus_node = AddOwnedChild("bus");
  if (bus_node.is_error()) {
    return bus_node.status_value();
  }
  bus_node_ = std::move(*bus_node);

  // Pass the root client to the allocators provided by Pci(e)Root. The initial root is
  // created to manage the start of the bus id range given to us by RootService.
  fbl::AllocChecker ac;
  root_ = std::unique_ptr<PciRoot>(new (&ac) PciRoot(info_.start_bus_num(), root_client_));
  if (!ac.check()) {
    fdf::error("failed to allocate root");
    return ZX_ERR_NO_MEMORY;
  }

  // Begin our bus scan starting at our root
  zx_status_t status = ScanDownstream();
  if (status != ZX_OK) {
    fdf::error("failed to scan downstream: {}", zx_status_get_string(status));
    return status;
  }
  status = ConfigureLegacyIrqs();
  if (status != ZX_OK) {
    fdf::error("error configuring legacy IRQs, they will be unavailable: {}",
               zx_status_get_string(status));
    return status;
  }
  root_->ConfigureDownstreamDevices();

  return ZX_OK;
}

zx::result<fidl::ClientEnd<fuchsia_driver_framework::NodeController>> Bus::AddDeviceNode(
    std::string_view name, fuchsia_hardware_pci::Service::InstanceHandler handler,
    std::span<const fuchsia_driver_framework::NodeProperty2> props,
    const fuchsia_driver_framework::BusInfo& bus_info,
    std::optional<fuchsia_driver_framework::CompositeNodeSpec> composite_spec) {
  zx::result service_result =
      outgoing()->AddService<fuchsia_hardware_pci::Service>(std::move(handler), name);
  if (service_result.is_error()) {
    fdf::error("Failed to add Service to outgoing directory: {}", service_result);
    return service_result.take_error();
  }

  auto cleanup = fit::defer([this, name] { RemoveDeviceService(name); });

  auto endpoints = fidl::CreateEndpoints<fuchsia_driver_framework::NodeController>();
  if (endpoints.is_error()) {
    return endpoints.take_error();
  }

  std::vector<fuchsia_driver_framework::NodeProperty2> properties{props.begin(), props.end()};
  std::vector<fuchsia_driver_framework::Offer> offers{
      fdf::MakeOffer2<fuchsia_hardware_pci::Service>(name),
  };

  fuchsia_driver_framework::NodeAddArgs args{{
      .name = {std::string(name)},
      .offers2 = std::move(offers),
      .bus_info = bus_info,
      .properties2 = std::move(properties),
  }};

  ZX_DEBUG_ASSERT(bus_node_.has_value());
  fidl::Result<fuchsia_driver_framework::Node::AddChild> result =
      fidl::Call(bus_node_->node_)->AddChild({std::move(args), std::move(endpoints->server), {}});
  if (result.is_error()) {
    fdf::error("Failed to add child node {}: {}", name, result.error_value().FormatDescription());
    return zx::error(result.error_value().is_framework_error()
                         ? result.error_value().framework_error().status()
                         : ZX_ERR_INTERNAL);
  }

  if (composite_spec.has_value()) {
    auto spec_result = composite_manager_->AddSpec(*std::move(composite_spec));
    if (spec_result.is_error()) {
      if (spec_result.error_value().is_framework_error()) {
        fdf::error("Failed to add composite node spec: {}",
                   spec_result.error_value().FormatDescription());
        [[maybe_unused]] auto _ = fidl::Call(endpoints->client)->Remove();
        return zx::error(spec_result.error_value().framework_error().status());
      }
      // Domain errors leave the device's node and service in place so its fragment and lspci
      // entry survive, as they did under DFv1. kAlreadyExists is expected when bus-pci restarts,
      // because specs outlive the driver that added them and can't be removed.
      if (spec_result.error_value().domain_error() !=
          fuchsia_driver_framework::CompositeNodeSpecError::kAlreadyExists) {
        fdf::warn("Failed to add composite node spec: {}",
                  spec_result.error_value().FormatDescription());
      }
    }
  }

  cleanup.cancel();
  return zx::ok(std::move(endpoints->client));
}

void Bus::RemoveDeviceService(std::string_view name) {
  [[maybe_unused]] auto result = outgoing()->RemoveService<fuchsia_hardware_pci::Service>(name);
}

// Maps a vmo as an mmio_buffer to be used as this Bus driver's ECAM region
// for config space access.
zx::result<fdf::MmioBuffer> Bus::MapConfigRegion(zx::vmo cam_vmo) {
  size_t size;
  zx_status_t status = cam_vmo.get_size(&size);
  if (status != ZX_OK) {
    fdf::error("couldn't get ecam vmo size: {}!", zx_status_get_string(status));
    return zx::error(status);
  }

  zx::result<fdf::MmioBuffer> result =
      fdf::MmioBuffer::Create(0, size, std::move(cam_vmo), ZX_CACHE_POLICY_UNCACHED_DEVICE);
  if (result.is_error()) {
    fdf::error("couldn't map ecam vmo: {}!", result);
    return result.take_error();
  }

  fdf::debug("ecam mapped at {:#x} (size: {:#x})", reinterpret_cast<uintptr_t>(result->get()),
             result->get_size());
  return zx::ok(std::move(*result));
}

zx::result<std::unique_ptr<Config>> Bus::MakeConfig(fuchsia_hardware_pci::wire::Bdf bdf) {
  if (ecam_) {
    return MmioConfig::Create(bdf, ecam_.value(), info_.start_bus_num(), info_.end_bus_num(),
                              is_extended_);
  }

  ZX_ASSERT(config_client_.is_valid());
  return ProxyConfig::Create(bdf, config_client_);
}

// Scan downstream starting at the bus id managed by the Bus's Root.
// In the process of scanning, take note of bridges found and configure any that are
// unconfigured. In the end the Bus should have a list of all devides, and all bridges
// should have a list of pointers to their own downstream devices.
zx_status_t Bus::ScanDownstream() {
  std::list<BusScanEntry> scan_list;
  // First scan the bus id associated with our root.
  BusScanEntry entry = {{static_cast<uint8_t>(root_->managed_bus_id()), 0, 0}, root_.get()};
  entry.upstream = root_.get();
  scan_list.push_back(entry);

  // Process any bridges found under the root, any bridges under those bridges, etc...
  // It's important that we scan in the order we discover bridges (DFS) because
  // when we implement bus id assignment it will affect the overall numbering
  // scheme of the bus topology.
  while (!scan_list.empty()) {
    auto entry = scan_list.back();
    fdf::trace("scanning from {:02x}:{:02x}.{:01x} upstream: {}", entry.bdf.bus_id,
               entry.bdf.device_id, entry.bdf.function_id,
               (entry.upstream->type() == UpstreamNode::Type::ROOT)
                   ? "root"
                   : static_cast<Bridge*>(entry.upstream)->config()->addr());
    // Remove this entry, otherwise we'll pop the wrong child off if the scan
    // adds any new bridges / resume points.
    scan_list.pop_back();
    ScanBus(entry, &scan_list);
  }

  return ZX_OK;
}

void Bus::ScanBus(BusScanEntry entry, std::list<BusScanEntry>* scan_list) {
  uint32_t bus_id = entry.bdf.bus_id;  // 32bit so bus_id won't overflow 8bit in the loop
  uint8_t _dev_id = entry.bdf.device_id;
  uint8_t _func_id = entry.bdf.function_id;
  uint8_t _max_functions = entry.max_functions;
  UpstreamNode* upstream = entry.upstream;
  for (uint8_t dev_id = _dev_id; dev_id < kMaxDevicesPerBus; dev_id++) {
    uint8_t max_functions = _max_functions;
    for (uint8_t func_id = _func_id; func_id < max_functions; func_id++) {
      fuchsia_hardware_pci::wire::Bdf bdf = {
          .bus_id = static_cast<uint8_t>(bus_id),
          .device_id = dev_id,
          .function_id = func_id,
      };
      zx::result<std::unique_ptr<Config>> config = MakeConfig(bdf);
      if (config.is_error()) {
        continue;
      }

      auto header_type = config::HeaderType.ReadFrom((*config).get());
      if (func_id == 0) {
        // Check if this device is multi-function. If it is, scan all 8 functions, otherwise, just
        // scan 1.
        if (header_type.multi_function()) {
          max_functions = kMaxFunctionsPerDevice;
        }
      }

      // Check that the device is valid by verifying the vendor and device ids.
      uint16_t vendor_id = config::VendorId.ReadFrom((*config).get()).reg_value();
      if (vendor_id == kInvalidVendorId) {
        continue;
      }

      bool is_bridge = (header_type.type() == kHeaderTypePciBridge);
      fdf::trace("\tfound {} at {:02x}:{:02x}.{:01x} VID {:#04x} DID {:#04x}",
                 is_bridge ? "bridge" : "device", bus_id, dev_id, func_id, vendor_id,
                 config::DeviceId.ReadFrom((*config).get()).reg_value());

      // If we found a bridge, add it to our bridge list and initialize /
      // enumerate it after we finish scanning this bus
      if (is_bridge) {
        fbl::RefPtr<Bridge> bridge;
        uint8_t mbus_id = config::SecondaryBusId.ReadFrom((*config).get()).reg_value();
        char addr[ZX_MAX_NAME_LEN];
        strncpy(addr, config->addr(), sizeof(addr));
        zx_status_t status = Bridge::Create(std::move(*config), upstream, this, mbus_id, &bridge);
        if (status != ZX_OK) {
          fdf::error("failed to create Bridge at {}: {}", addr, zx_status_get_string(status));
          continue;
        }

        // Create scan entries for the next device we would have looked
        // at in the current level of the tree, as well as the new
        // bridge. Since we always work our way from the top of the scan
        // stack we effectively scan the bus in a DFS manner. |func_id|
        // is always incremented by one to ensure we don't scan this
        // same bdf again. If the incremented value is invalid then the
        // device_id loop will iterate and we'll be in a good state
        // again.
        BusScanEntry resume_entry{};
        resume_entry.bdf.bus_id = static_cast<uint8_t>(bus_id);
        resume_entry.bdf.device_id = dev_id;
        resume_entry.bdf.function_id = static_cast<uint8_t>(func_id + 1);
        resume_entry.upstream = upstream;
        resume_entry.max_functions = max_functions;
        scan_list->push_back(resume_entry);

        BusScanEntry bridge_entry{};
        bridge_entry.bdf.bus_id = static_cast<uint8_t>(bridge->managed_bus_id());
        bridge_entry.upstream = bridge.get();  // the new bridge will be this scan's upstream
        scan_list->push_back(bridge_entry);
        // Quit this scan and pick up again based on the scan entries found.
        return;
      }

      // We're at a leaf node in the topology so create a normal device.
      char addr[ZX_MAX_NAME_LEN];
      strncpy(addr, config->addr(), sizeof(addr));
      zx_status_t status =
          pci::Device::Create(std::move(*config), upstream, this,
                              /*has_acpi=*/DeviceHasAcpi(config->bdf()),
                              /*has_devicetree=*/DeviceHasDevicetree(config->bdf()));
      if (status != ZX_OK) {
        fdf::error("failed to create device at {}: {}", addr, zx_status_get_string(status));
      }
    }

    // Reset _func_id and _max_functions so the next device will be scanned starting from function 0
    // with the default single-function assumption
    _func_id = 0;
    _max_functions = BusScanEntry::kDefaultMaxFunctions;
  }
}

bool Bus::DeviceHasAcpi(fuchsia_hardware_pci::wire::Bdf bdf) {
  if (!info_.parent_bdfs().has_value() ||
      info_.parent_bdfs()->Which() != fuchsia_hardware_pci_root::ParentBdfs::Tag::kAcpi) {
    return false;
  }
  const auto& bdfs = info_.parent_bdfs()->acpi().value();
  auto find_fn = [&bdf](const fuchsia_hardware_pci::Bdf& entry) -> bool {
    return bdf.bus_id == entry.bus_id() && bdf.device_id == entry.device_id() &&
           bdf.function_id == entry.function_id();
  };
  return std::ranges::find_if(bdfs, find_fn) != std::end(bdfs);
}

bool Bus::DeviceHasDevicetree(fuchsia_hardware_pci::wire::Bdf bdf) {
  if (!info_.parent_bdfs().has_value() ||
      info_.parent_bdfs()->Which() != fuchsia_hardware_pci_root::ParentBdfs::Tag::kDevicetree) {
    return false;
  }
  const auto& bdfs = info_.parent_bdfs()->devicetree().value();
  auto find_fn = [&bdf](const fuchsia_hardware_pci::Bdf& entry) -> bool {
    return bdf.bus_id == entry.bus_id() && bdf.device_id == entry.device_id() &&
           bdf.function_id == entry.function_id();
  };
  return std::ranges::find_if(bdfs, find_fn) != std::end(bdfs);
}

zx_status_t Bus::SetUpLegacyIrqHandlers() {
  // Most cases devices will use MSI / MSI-X anyway so warnings are sufficient on bind failure.
  std::set<uint32_t> vectors;
  for (const auto& entry : info_.irq_routing()) {
    for (uint8_t pin : entry.pins()) {
      if (pin != 0) {
        vectors.insert(pin);
      }
    }
  }

  for (uint32_t vector : vectors) {
    fidl::WireResult result = root_client_->GetLegacyInterrupt(vector);
    if (!result.ok()) {
      fdf::warn("failed to send GetLegacyInterrupt request: {}", result.status_string());
      continue;
    }
    if (result->is_error()) {
      fdf::warn("legacy interrupt vector {:#x} unavailable: {}", vector,
                zx_status_get_string(result->error_value()));
      continue;
    }

    fbl::AllocChecker ac;
    auto shared_vector = std::unique_ptr<SharedVector>(new (&ac) SharedVector());
    if (!ac.check()) {
      fdf::error("failed to allocate shared vector for irq {:#x}", vector);
      return ZX_ERR_NO_MEMORY;
    }
    shared_vector->interrupt = std::move(result->value()->interrupt);
    shared_vector->irq_handler.set_object(shared_vector->interrupt.get());
    shared_vector->irq_handler.set_handler([this, vector](async_dispatcher_t* dispatcher,
                                                          async::Irq* irq, zx_status_t status,
                                                          const zx_packet_interrupt_t* interrupt) {
      HandleLegacyIrq(dispatcher, irq, status, interrupt, vector);
    });

    zx_status_t status = shared_vector->irq_handler.Begin(dispatcher_);
    if (status != ZX_OK) {
      fdf::warn("failed to bind irq {:#x} to dispatcher: {}", vector, zx_status_get_string(status));
      return status;
    }

    // Every vector has a list of devices associated with it that are wired to that IRQ.
    shared_irqs_[vector] = std::move(shared_vector);
  }

  return ZX_OK;
}

zx_status_t Bus::ConfigureLegacyIrqs() {
  fbl::AutoLock devices_lock(&devices_lock_);
  zx_status_t status = SetUpLegacyIrqHandlers();
  if (status != ZX_OK) {
    return status;
  }

  // Scan all the devices found and figure out their interrupt pin based on the
  // routing table provided by the platform. While we hold the devices_lock no
  // changes can be made to the Bus topology, ensuring the lifetimes of the
  // upstream paths and config accesses.
  for (const auto& [bdf, device] : devices_) {
    uint8_t pin = config::InterruptPin.ReadFrom(device->config().get()).reg_value();
    // If a device has no pin configured in the InterruptPin register then it
    // has no legacy interrupt. PCI Local Bus Spec v3 Section 2.2.6.
    if (pin == 0 && board_config_.use_intx_workaround().has_value()) {
      pin = 1;  // TODO(365837900): Re-add 'continue' here when crosvm Interrupt Pin is populated.
    }
    if (pin == 0 || pin > kMaxLegacyIrqPins) {
      continue;
    }

    // To avoid devices all ending up on the same pin the PCI bridge spec
    // defines a transformation per pin based on the device id of a given function
    // and pin. This transformation is applied at every transition from a
    // secondary bus to a primary bus up to the root. In effect, we swizzle the
    // pin every time we find a bridge working our way back up to the root. At
    // the same time, we also want to record the bridge closest to the root in
    // case it is a root port so that we can check the correct irq routing table
    // entries.
    //
    // Pci Bridge to Bridge spec r1.2 Table 9-1
    // PCI Express Base Specification r4.0 Table 2-19
    UpstreamNode* upstream = device->upstream();
    std::optional<fuchsia_hardware_pci::wire::Bdf> port;
    while (upstream && upstream->type() == UpstreamNode::Type::BRIDGE) {
      pin = (pin + device->dev_id()) % kMaxLegacyIrqPins;
      auto bridge = static_cast<pci::Bridge*>(upstream);
      port = bridge->config()->bdf();
      upstream = bridge->upstream();
    }
    ZX_DEBUG_ASSERT(upstream);
    ZX_DEBUG_ASSERT(upstream->type() == UpstreamNode::Type::ROOT);

    // If we didn't find a parent then the device must be a root complex endpoint.
    if (!port) {
      port = {
          .device_id = static_cast<uint8_t>(fuchsia_hardware_pci_root::kIrqRoutingNoParent),
          .function_id = static_cast<uint8_t>(fuchsia_hardware_pci_root::kIrqRoutingNoParent),
      };
    }

    // There must be a routing entry for a given device / root port combination
    // in order for a device's legacy IRQ to work. Attempt to find it and use
    // the newly swizzled pin value to find the hardware vector.
    auto find_fn = [&device, port](const auto& entry) -> bool {
      return entry.port_device_id() == port->device_id &&
             entry.port_function_id() == port->function_id && entry.device_id() == device->dev_id();
    };

    auto found = std::find_if(info_.irq_routing().begin(), info_.irq_routing().end(), find_fn);
    if (found != std::end(info_.irq_routing())) {
      uint8_t vector = found->pins()[pin - 1];
      config::InterruptLine.FromValue(vector).WriteTo(device->config().get());
      fdf::debug("[{}] pin {} mapped to {:#x}", device->config()->addr(), pin, vector);
    } else {
      fdf::debug("[{}] no legacy routing entry found for device", device->config()->addr());
    }
  }

  return ZX_OK;
}

void Bus::Stop(fdf::StopCompleter completer) {
  Teardown();
  completer(zx::ok());
}

void Bus::Teardown() {
  if (root_) {
    root_->DisableDownstream();
    root_->UnplugDownstream();
    root_.reset();
  }
  bus_node_.reset();
}

Bus::~Bus() { Teardown(); }

void Bus::HandleDeviceLegacyIrqUntriggered(async_dispatcher_t* /*dispatcher*/,
                                           async::Wait* /*wait*/, zx_status_t status,
                                           const zx_packet_signal_t* /*signal*/,
                                           pci::Device* device) {
  if (status == ZX_ERR_CANCELED) {
    return;
  }
  if (status != ZX_OK) {
    fdf::error("Error waiting on legacy irq untriggered for {}: {}", device->config()->addr(),
               zx_status_get_string(status));
    return;
  }
  fbl::AutoLock dev_lock(device->dev_lock());
  if (device->irqs().mode != fuchsia_hardware_pci::InterruptMode::kLegacy) {
    return;
  }
  device->EnableLegacyIrq();
}

void Bus::HandleLegacyIrq(async_dispatcher_t* dispatcher, async::Irq* irq, zx_status_t status,
                          const zx_packet_interrupt_t* interrupt, uint32_t vector) {
  if (status == ZX_ERR_CANCELED) {
    return;
  }
  if (status != ZX_OK || !interrupt) {
    fdf::error("Unexpected error in IRQ handling for vector {:#x}, status = {}", vector,
               zx_status_get_string(status));
    return;
  }

  // This is effectively our 'fast path'. We've received an interrupt packet
  // and we need to scan the list for devices mapped to that vector to see
  // which ones have an interrupt asserted in their status register. In a
  // typical situation a bus driver is required to check if a driver has
  // interrupts enabled and if the status bit is asserted. However, in our
  // case if a device exists in this list it was only through enabling
  // legacy IRQs, ensuring that interrupts are enabled. We can save a config
  // read and just check status thanks to this.
  fbl::AutoLock devices_lock(&devices_lock_);
  auto result = shared_irqs_.find(vector);
  if (result == shared_irqs_.end()) {
    fdf::error("Received IRQ for untracked vector {:#x}", vector);
    return;
  }
  SharedVector& shared_vector = *result->second;
  SharedDeviceList& list = shared_vector.list;
  for (auto& entry : list) {
    pci::Device* device = entry->device;
    fbl::AutoLock device_lock(device->dev_lock());
    if (entry->wait.is_pending() || device->irqs().legacy_disabled) {
      continue;
    }
    auto dev_status = config::Status.ReadFrom(device->config().get());
    if (dev_status.interrupt_status() || board_config_.use_intx_workaround()) {
      // Legacy (INTx) interrupts are level-triggered: the device holds its
      // interrupt line asserted until its driver services the device and
      // clears the source. Mask the device's interrupt (set INT_DISABLE) so
      // the line deasserts before we re-arm the physical interrupt below,
      // preventing interrupt storms.
      device->DisableLegacyIrq();

      // Trigger the virtual interrupt the device driver is using by proxy.
      zx_status_t signal_status = device->SignalLegacyIrq(interrupt->timestamp);
      if (signal_status != ZX_OK) {
        fdf::error("failed to signal vector {:#x} for device {}: {}", vector,
                   device->config()->addr(), zx_status_get_string(signal_status));
        // If signaling fails (e.g. the client driver closed or destroyed its virtual
        // interrupt handle), the device intentionally remains masked to prevent
        // physical interrupt storms.
        continue;
      }

      zx_status_t wait_status = entry->wait.Begin(dispatcher_);
      if (wait_status != ZX_OK) {
        fdf::error("failed to begin wait on untriggered for {}: {}", device->config()->addr(),
                   zx_status_get_string(wait_status));
      }
    }
  }

  // Re-arm the physical interrupt for subsequent deliveries now that all asserting
  // devices sharing this vector have been masked.
  zx_status_t ack_status = shared_vector.interrupt.ack();
  if (ack_status != ZX_OK) {
    fdf::error("Failed to ack vector {:#x} after servicing device: {}", vector,
               zx_status_get_string(ack_status));
  }
}

}  // namespace pci
