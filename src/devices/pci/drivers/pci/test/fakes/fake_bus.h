// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_DEVICES_PCI_DRIVERS_PCI_TEST_FAKES_FAKE_BUS_H_
#define SRC_DEVICES_PCI_DRIVERS_PCI_TEST_FAKES_FAKE_BUS_H_

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.hardware.pci.root/cpp/wire.h>
#include <fidl/fuchsia.hardware.pci/cpp/wire.h>
#include <lib/driver/mmio/cpp/mmio-buffer.h>
#include <lib/driver/outgoing/cpp/outgoing_directory.h>
#include <lib/zx/event.h>
#include <lib/zx/msi.h>
#include <lib/zx/resource.h>

#include <fbl/auto_lock.h>
#include <fbl/mutex.h>
#include <fbl/ref_ptr.h>
#include <hwreg/bitfields.h>

#include "src/devices/pci/drivers/pci/bus.h"
#include "src/devices/pci/drivers/pci/bus_device_interface.h"
#include "src/devices/pci/drivers/pci/device.h"
#include "src/devices/pci/drivers/pci/test/fakes/fake_ecam.h"

namespace pci {

class FakeBus : public BusDeviceInterface {
 public:
  static constexpr uint64_t kDefaultNonKernelMsiTargetAddr = 0x1000;
  static constexpr uint32_t kDefaultNonKernelMsiTargetData = 0x10;

  explicit FakeBus(uint8_t bus_start, uint8_t bus_end, bool is_extended, uint16_t segment_group = 0)
      : ecam_(bus_start, bus_end, is_extended), segment_group_(segment_group) {}

  zx_status_t LinkDevice(fbl::RefPtr<pci::Device> device) final {
    fbl::AutoLock devices_lock(&devices_lock_);
    return devices_.try_emplace(device->config()->bdf(), std::move(device)).second
               ? ZX_OK
               : ZX_ERR_ALREADY_EXISTS;
  }

  zx_status_t UnlinkDevice(pci::Device* device) final {
    fbl::AutoLock devices_lock(&devices_lock_);
    return devices_.erase(device->config()->bdf()) > 0 ? ZX_OK : ZX_ERR_NOT_FOUND;
  }

  zx_status_t AllocateMsi(uint32_t count, zx::msi* out_allocation,
                          fuchsia_hardware_pci_root::wire::MsiAllocationInfo* out_info) final {
    fbl::AutoLock devices_lock(&devices_lock_);
    if (use_non_kernel_msi_allocation_) {
      zx::event event;
      if (zx_status_t status = zx::event::create(0, &event); status != ZX_OK) {
        return status;
      }
      *out_allocation = zx::msi(event.release());
      out_info->target_addr = non_kernel_msi_target_addr_;
      out_info->target_data = kDefaultNonKernelMsiTargetData;
      out_info->irq_count = count;
      return ZX_OK;
    }
    zx_status_t status =
        zx_msi_allocate(ZX_HANDLE_INVALID, count, out_allocation->reset_and_get_address());
    if (status != ZX_OK) {
      return status;
    }
    out_info->target_addr = 0xCAFE;
    out_info->target_data = 0xC0FE;
    out_info->irq_count = count;
    return ZX_OK;
  }

  zx_status_t GetMsiHandle(const zx::msi& allocation, uint32_t options, uint16_t msi_id,
                           const zx::vmo& cfg_vmo, uint64_t cfg_offset,
                           zx::interrupt* out_interrupt) final {
    fbl::AutoLock devices_lock(&devices_lock_);
    if (use_non_kernel_msi_allocation_) {
      return zx::interrupt::create(*zx::unowned_resource(ZX_HANDLE_INVALID), 0,
                                   ZX_INTERRUPT_VIRTUAL, out_interrupt);
    }
    return zx::msi::create(allocation, options, msi_id, cfg_vmo, cfg_offset, out_interrupt);
  }

  void set_use_non_kernel_msi_allocation(bool enable,
                                         uint64_t target_addr = kDefaultNonKernelMsiTargetAddr) {
    fbl::AutoLock devices_lock(&devices_lock_);
    use_non_kernel_msi_allocation_ = enable;
    non_kernel_msi_target_addr_ = target_addr;
  }

  zx_status_t GetBti(const pci::Device* /*device*/, uint32_t /*index*/, zx::bti* /*bti*/) final {
    fbl::AutoLock devices_lock(&devices_lock_);
    return ZX_ERR_NOT_SUPPORTED;
  }

  uint16_t GetSegmentGroup() final { return segment_group_; }

  zx_status_t AddToSharedIrqList(pci::Device* device, uint32_t vector,
                                 zx::unowned_interrupt /*irq_handle*/) final {
    return ZX_OK;
  }
  zx_status_t RemoveFromSharedIrqList(pci::Device* device, uint32_t vector) final { return ZX_OK; }

  zx::result<fidl::ClientEnd<fuchsia_driver_framework::NodeController>> AddDeviceNode(
      std::string_view name, fuchsia_hardware_pci::Service::InstanceHandler handler,
      std::span<const fuchsia_driver_framework::NodeProperty2> props,
      const fuchsia_driver_framework::BusInfo& bus_info,
      std::optional<fuchsia_driver_framework::CompositeNodeSpec> composite_spec) final {
    if (composite_spec.has_value()) {
      composite_spec_count_++;
    }

    if (outgoing_) {
      zx::result result =
          outgoing_->AddService<fuchsia_hardware_pci::Service>(std::move(handler), name);
      if (result.is_error()) {
        return result.take_error();
      }
    }

    return zx::ok(fidl::ClientEnd<fuchsia_driver_framework::NodeController>{});
  }

  void RemoveDeviceService(std::string_view name) final {
    if (outgoing_) {
      [[maybe_unused]] auto result = outgoing_->RemoveService<fuchsia_hardware_pci::Service>(name);
    }
  }

  // Publishes device services to |outgoing| and serves their bindings on |dispatcher|. Unit tests
  // that never connect to a device service leave both unset.
  void SetDriverContext(fdf::OutgoingDirectory* outgoing, async_dispatcher_t* dispatcher) {
    outgoing_ = outgoing;
    dispatcher_ = dispatcher;
  }

  async_dispatcher_t* binding_dispatcher() override { return dispatcher_; }

  uint32_t composite_spec_count() const { return composite_spec_count_; }

  pci::Device& get_device(fuchsia_hardware_pci::wire::Bdf bdf) { return *devices_.at(bdf); }

  // For use with Devices that need to link to a Bus.
  BusDeviceInterface* bdi() { return static_cast<BusDeviceInterface*>(this); }

  const pci::DeviceTree& devices() { return devices_; }
  FakeEcam& ecam() { return ecam_; }
  bool is_extended() const { return ecam_.is_extended(); }

 private:
  // Devices access config space through ecam_, so ecam_ must outlive devices_.
  FakeEcam ecam_;
  fbl::Mutex devices_lock_;
  pci::DeviceTree devices_;
  uint16_t segment_group_;
  bool use_non_kernel_msi_allocation_ __TA_GUARDED(devices_lock_) = false;
  uint64_t non_kernel_msi_target_addr_ __TA_GUARDED(devices_lock_) = kDefaultNonKernelMsiTargetAddr;
  fdf::OutgoingDirectory* outgoing_ = nullptr;
  async_dispatcher_t* dispatcher_ = nullptr;
  uint32_t composite_spec_count_ = 0;
};

}  // namespace pci

#endif  // SRC_DEVICES_PCI_DRIVERS_PCI_TEST_FAKES_FAKE_BUS_H_
