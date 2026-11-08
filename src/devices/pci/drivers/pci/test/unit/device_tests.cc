// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/testing/cpp/scoped_global_logger.h>
#include <lib/zx/clock.h>
#include <lib/zx/time.h>
#include <unistd.h>
#include <zircon/limits.h>
#include <zircon/syscalls/object.h>

#include <algorithm>
#include <cstring>
#include <memory>
#include <utility>
#include <vector>

#include <fbl/auto_lock.h>
#include <fbl/ref_ptr.h>
#include <gtest/gtest.h>

#include "src/devices/pci/drivers/pci/bridge.h"
#include "src/devices/pci/drivers/pci/device.h"
#include "src/devices/pci/drivers/pci/test/fakes/fake_allocator.h"
#include "src/devices/pci/drivers/pci/test/fakes/fake_bus.h"
#include "src/devices/pci/drivers/pci/test/fakes/fake_config.h"
#include "src/devices/pci/drivers/pci/test/fakes/fake_upstream_node.h"
#include "src/devices/pci/drivers/pci/test/fakes/test_device.h"
#include "src/devices/pci/lib/cpp/capabilities.h"
#include "src/devices/pci/lib/cpp/capabilities/pci_express.h"
#include "src/devices/pci/lib/cpp/capabilities/power_management.h"
#include "src/devices/pci/lib/cpp/config.h"
#include "src/lib/testing/predicates/status.h"

namespace pci {

// Creates a test device with a given device config using test defaults)

template <bool IsExtended, UpstreamNode::Type UpstreamType = UpstreamNode::Type::ROOT>
class PciDeviceTests : public ::testing::Test {
 public:
  static fuchsia_hardware_pci::wire::Bdf bdf() {
    return {.bus_id = 1, .device_id = 2, .function_id = 3};
  }

 protected:
  FakeBus& bus() { return bus_; }
  FakeUpstreamNode& upstream() { return upstream_; }

  Device& CreateTestDevice() {
    auto& ecam = bus_.ecam();
    auto mmio_cfg = ecam.CreateMmioConfig(bdf());
    auto view = mmio_cfg->get_view();
    EXPECT_TRUE(view.is_ok());
    auto fake_cfg = std::make_unique<FakeMmioConfig>(bdf(), std::move(view.value()));
    // Create and initialize the fake device.
    EXPECT_OK(Device::Create(std::move(fake_cfg), &upstream(), &bus(),
                             /*has_acpi=*/false, /*has_devicetree=*/false));
    return bus().get_device(bdf());
  }
  Device& CreateTestDevice(const uint8_t* cfg_buf, size_t cfg_size) {
    bus_.ecam().get_config_view(bdf()).WriteBuffer(0, cfg_buf, cfg_size);
    return CreateTestDevice();
  }
  void ConfigureDownstreamDevices() { return upstream_.ConfigureDownstreamDevices(); }

  void ExpectMsiState(const Device& dev, bool enabled) {
    auto* msi = dev.capabilities().msi;
    ASSERT_NE(msi, nullptr);
    EXPECT_EQ(msi->ctrl().ReadFrom(dev.config().get()).enable(), enabled ? 1u : 0u);
  }

  void ExpectMsixCtrl(const Device& dev, bool enabled, bool function_mask) {
    auto* msix = dev.capabilities().msix;
    ASSERT_NE(msix, nullptr);
    auto msix_ctrl = msix->ctrl().ReadFrom(dev.config().get());
    EXPECT_EQ(msix_ctrl.enable(), enabled ? 1u : 0u);
    EXPECT_EQ(msix_ctrl.function_mask(), function_mask ? 1u : 0u);
  }

  PciDeviceTests()
      : bus_(/*bus_start=*/0, /*bus_end=*/2, /*is_extended=*/IsExtended),
        upstream_(UpstreamType, 0) {}
  ~PciDeviceTests() override {
    upstream_.DisableDownstream();
    upstream_.UnplugDownstream();
  }

 private:
  fdf_testing::ScopedGlobalLogger logger_;
  FakeBus bus_;
  FakeUpstreamNode upstream_;
};

using PciDeviceTestsCam = PciDeviceTests<false>;
using PciDeviceTestsExtendedCam = PciDeviceTests<true>;
using PciDeviceTestExtendedCamBridge = PciDeviceTests<true, UpstreamNode::Type::BRIDGE>;

// All tests below
namespace {

TEST_F(PciDeviceTestsExtendedCam, CreationTest) {
  auto& ecam = bus().ecam();
  // This test creates a device, goes through its init sequence, links it into
  // the toplogy, and then has it linger. It will be cleaned up by TearDown()
  // releasing all objects of upstream(). If creation succeeds here and no
  // asserts happen following the test it means the fakes are built properly
  // enough and the basic interface is fulfilled.
  ecam.get_config_view(bdf()).WriteBuffer(0, kFakeQuadroDeviceConfig.data(),
                                          kFakeQuadroDeviceConfig.max_size());
  auto mmio_cfg = MmioConfig::Create(bdf(), ecam.mmio(), ecam.bus_start(),
                                     ecam.bus_start() + ecam.bus_cnt(), ecam.is_extended());
  ASSERT_OK(mmio_cfg.status_value());
  auto view = mmio_cfg->get_view();
  EXPECT_TRUE(view.is_ok());
  auto fake_cfg = std::make_unique<FakeMmioConfig>(bdf(), std::move(view.value()));
  ASSERT_OK(Device::Create(std::move(fake_cfg), &upstream(), &bus(), /*has_acpi=*/false,
                           /*has_devicetree=*/false));

  upstream().fake_mmio_regions().FailNextAllocation(true);
  upstream().fake_pf_mmio_regions().FailNextAllocation(true);
  ConfigureDownstreamDevices();

  // Verify the created device's BDF.
  auto& dev = bus().get_device(bdf());
  ASSERT_EQ(bdf().bus_id, dev.bus_id());
  ASSERT_EQ(bdf().device_id, dev.dev_id());
  ASSERT_EQ(bdf().function_id, dev.func_id());
  ASSERT_EQ(0u, dev.segment_group());
  EXPECT_STREQ("01_02_3", dev.node_name());
  EXPECT_STREQ("01:02.3", dev.config()->addr());
}

// Verify BARs are allocated in size-descending order so that a large BAR
// cannot be blocked by a small BAR fragmenting the available address window.
TEST_F(PciDeviceTestsExtendedCam, BarAllocationSizeDescendingOrder) {
  [[maybe_unused]] auto& dev =
      CreateTestDevice(kFakeQuadroDeviceConfig.data(), kFakeQuadroDeviceConfig.max_size());
  ASSERT_EQ(false, HasNonfatalFailure());
  ConfigureDownstreamDevices();

  const auto& allocation_log = upstream().fake_mmio_regions().allocation_log();

  // BAR 0 is pre-assigned so it is allocated first, and the remaining MMIO BARs of
  // |kFakeQuadroDeviceConfig| follow in size-descending order (BAR 1, 3, 2).
  const std::vector<AllocationLogEntry> expected = {
      {.size = kTestDeviceBars[0].size, .succeeded = true},
      {.size = kTestDeviceBars[1].size, .succeeded = true},
      {.size = kTestDeviceBars[3].size, .succeeded = true},
      {.size = kTestDeviceBars[2].size, .succeeded = true},
  };
  EXPECT_EQ(allocation_log, expected);
}
TEST_F(PciDeviceTestsExtendedCam, PreassignedBars) {
  auto& ecam = bus().ecam();
  ecam.get_config_view(bdf()).WriteBuffer(0, kFakeQuadroDeviceConfig.data(),
                                          kFakeQuadroDeviceConfig.max_size());
  auto mmio_cfg = ecam.CreateMmioConfig(bdf());
  // Configure BAR 3 (64-bit prefetchable MMIO, size 32M) with a preassigned 64-bit address
  // (0x1E0000000) while BAR 0 (32-bit MMIO @ 0x800000) and BAR 5 (I/O @ 0x2000) are already
  // preassigned in kFakeQuadroDeviceConfig with I/O and memory decoding enabled.
  config::BaseAddress(3)
      .FromValue(0)
      .mmio()
      .set_base_address(0xE0000000)
      .set_type(config::MmioBaseAddressReg::kType64Bit)
      .set_is_prefetchable(1)
      .WriteTo(mmio_cfg.get());
  config::BaseAddress(4).FromValue(0x00000001).WriteTo(mmio_cfg.get());

  auto& dev = CreateTestDevice();
  ConfigureDownstreamDevices();

  fbl::AutoLock dev_lock(dev.dev_lock());

  const auto& bar0_info = dev.bars()[0];
  ASSERT_TRUE(bar0_info.has_value());
  EXPECT_TRUE(bar0_info->is_mmio);
  EXPECT_FALSE(bar0_info->is_64bit);
  EXPECT_FALSE(bar0_info->is_prefetchable);
  EXPECT_EQ(bar0_info->address, 0x00800000u);
  EXPECT_EQ(bar0_info->size, kTestDeviceBars[0].size);

  const auto& bar3_info = dev.bars()[3];
  ASSERT_TRUE(bar3_info.has_value());
  EXPECT_FALSE(dev.bars()[4].has_value());
  EXPECT_TRUE(bar3_info->is_mmio);
  EXPECT_TRUE(bar3_info->is_64bit);
  EXPECT_TRUE(bar3_info->is_prefetchable);
  EXPECT_EQ(bar3_info->address, 0x00000001E0000000ULL);
  EXPECT_EQ(bar3_info->size, kTestDeviceBars[3].size);

  const auto& bar5_info = dev.bars()[5];
  ASSERT_TRUE(bar5_info.has_value());
  EXPECT_FALSE(bar5_info->is_mmio);
  EXPECT_FALSE(bar5_info->is_64bit);
  EXPECT_FALSE(bar5_info->is_prefetchable);
  EXPECT_EQ(bar5_info->address, 0x2000u);
  EXPECT_EQ(bar5_info->size, kTestDeviceBars[5].size);

  auto bar0 = config::BaseAddress(0).ReadFrom(dev.config().get());
  EXPECT_FALSE(bar0.is_io_space());
  EXPECT_EQ(bar0.mmio().base_address(), 0x00800000u);

  auto bar3 = config::BaseAddress(3).ReadFrom(dev.config().get());
  EXPECT_FALSE(bar3.is_io_space());
  EXPECT_EQ(bar3.mmio().type(), config::MmioBaseAddressReg::kType64Bit);
  EXPECT_TRUE(bar3.mmio().is_prefetchable());
  EXPECT_EQ(bar3.mmio().base_address(), 0xE0000000u);
  EXPECT_EQ(config::BaseAddress(4).ReadFrom(dev.config().get()).reg_value(), 0x00000001u);

  auto bar5 = config::BaseAddress(5).ReadFrom(dev.config().get());
  EXPECT_TRUE(bar5.is_io_space());
  EXPECT_EQ(bar5.io().base_address(), 0x2000u);

  auto cmd = config::Command.ReadFrom(dev.config().get());
  EXPECT_TRUE(cmd.memory_space());
  EXPECT_TRUE(cmd.io_space());
}

// Test a normal capability chain
TEST_F(PciDeviceTestsCam, StdCapabilityTest) {
  // Copy the config dump into a device entry in the ecam.
  auto& ecam = bus().ecam();
  ecam.get_config_view(bdf()).WriteBuffer(0, kFakeVirtioInputDeviceConfig.data(),
                                          kFakeVirtioInputDeviceConfig.max_size());
  auto cfg = ecam.CreateMmioConfig(bdf());
  ASSERT_OK(Device::Create(std::move(cfg), &upstream(), &bus(),
                           /*has_acpi=*/false, /*has_devicetree=*/false));
  auto& dev = bus().get_device(bdf());

  // Ensure our faked Keyboard exists.
  ASSERT_EQ(0x1af4u, dev.vendor_id());
  ASSERT_EQ(0x1052u, dev.device_id());

  // Since this is a dump of an emulated device we know it has a single MSI-X
  // capability followed by five Vendor capabilities.
  const auto& list = dev.capabilities().list;
  ASSERT_EQ(list.size(), 6u);
  EXPECT_EQ(static_cast<Capability::Id>(list[0]->id()), Capability::Id::kMsiX);
  EXPECT_EQ(static_cast<Capability::Id>(list[1]->id()), Capability::Id::kVendor);
  EXPECT_EQ(static_cast<Capability::Id>(list[2]->id()), Capability::Id::kVendor);
  EXPECT_EQ(static_cast<Capability::Id>(list[3]->id()), Capability::Id::kVendor);
  EXPECT_EQ(static_cast<Capability::Id>(list[4]->id()), Capability::Id::kVendor);
  EXPECT_EQ(static_cast<Capability::Id>(list[5]->id()), Capability::Id::kVendor);
}

// A device that is described by the devicetree publishes its fragment but not a
// composite node spec: the devicetree (pci-child-visitor) owns the composite.
TEST_F(PciDeviceTestsCam, DevicetreeDeviceSkipsCompositeSpec) {
  auto& ecam = bus().ecam();
  ecam.get_config_view(bdf()).WriteBuffer(0, kFakeVirtioInputDeviceConfig.data(),
                                          kFakeVirtioInputDeviceConfig.max_size());
  auto cfg = ecam.CreateMmioConfig(bdf());

  ASSERT_OK(Device::Create(std::move(cfg), &upstream(), &bus(),
                           /*has_acpi=*/false, /*has_devicetree=*/true));
  EXPECT_EQ(bus().composite_spec_count(), 0u);
}

// A device that is not described by the devicetree gets its composite from the
// bus driver, as before.
TEST_F(PciDeviceTestsCam, NonDevicetreeDeviceAddsCompositeSpec) {
  auto& ecam = bus().ecam();
  ecam.get_config_view(bdf()).WriteBuffer(0, kFakeVirtioInputDeviceConfig.data(),
                                          kFakeVirtioInputDeviceConfig.max_size());
  auto cfg = ecam.CreateMmioConfig(bdf());

  ASSERT_OK(Device::Create(std::move(cfg), &upstream(), &bus(),
                           /*has_acpi=*/false, /*has_devicetree=*/false));
  EXPECT_EQ(bus().composite_spec_count(), 1u);
}

// Test an extended capability chain
TEST_F(PciDeviceTestsExtendedCam, ExtendedCapabilityTest) {
  auto& dev = CreateTestDevice(kFakeQuadroDeviceConfig.data(), kFakeQuadroDeviceConfig.max_size());
  ASSERT_EQ(false, HasNonfatalFailure());

  // Since this is a dump of an emulated device we that it should have:
  //
  //      Capabilities: [100] Virtual Channel
  //      Capabilities: [250] Latency Tolerance Reporting
  //      Capabilities: [258] L1 PM Substates
  //      Capabilities: [128] Power Budgeting
  //      Capabilities: [600] Vendor Specific Information
  const auto& ext_list = dev.capabilities().ext_list;
  ASSERT_EQ(ext_list.size(), 5u);
  EXPECT_EQ(static_cast<ExtCapability::Id>(ext_list[0]->id()),
            ExtCapability::Id::kVirtualChannelNoMFVC);
  EXPECT_EQ(static_cast<ExtCapability::Id>(ext_list[1]->id()),
            ExtCapability::Id::kLatencyToleranceReporting);
  EXPECT_EQ(static_cast<ExtCapability::Id>(ext_list[2]->id()), ExtCapability::Id::kL1PMSubstates);
  EXPECT_EQ(static_cast<ExtCapability::Id>(ext_list[3]->id()), ExtCapability::Id::kPowerBudgeting);
  EXPECT_EQ(static_cast<ExtCapability::Id>(ext_list[4]->id()), ExtCapability::Id::kVendor);
}

// This test checks for proper handling of capability pointers that are
// invalid by pointing to inside the config header.
TEST_F(PciDeviceTestsCam, InvalidPtrCapabilityTest) {
  FakeBus bus(0, 2, true);
  FakeEcam& ecam = bus.ecam();
  auto raw_cfg = bus.ecam().get_config_view(bdf());
  auto* fake_dev = bus.ecam().get_device(bdf());

  // Two valid locations, followed by a third capability pointing at BAR 1.
  const uint8_t kCap1 = 0x80;
  const uint8_t kCap2 = 0x90;
  const uint8_t kInvalidCap = 0x10;

  // Point to 0x80 as the first capability.
  fake_dev->set_vendor_id(0x8086)
      .set_device_id(0x1234)
      .set_capabilities_list(1)
      .set_capabilities_ptr(kCap1);
  raw_cfg.Write(static_cast<uint8_t>(Capability::Id::kPciPowerManagement), kCap1);
  raw_cfg.Write(kCap2, kCap1 + 1);
  raw_cfg.Write(static_cast<uint8_t>(Capability::Id::kMsiX), kCap2);
  raw_cfg.Write(kInvalidCap, kCap2 + 1);

  auto cfg = MmioConfig::Create(bdf(), ecam.mmio(), ecam.bus_start(), 2, ecam.is_extended());
  ASSERT_OK(cfg.status_value());
  EXPECT_EQ(ZX_ERR_OUT_OF_RANGE, Device::Create(std::move(cfg.value()), &upstream(), &bus,
                                                /*has_acpi=*/false, /*has_devicetree=*/false));

  // Ensure no device was added.
  EXPECT_TRUE(bus.devices().empty());
}

// This test checks for proper handling (ZX_ERR_BAD_STATE) upon
// funding a pointer cycle while parsing capabilities.
TEST_F(PciDeviceTestsCam, PtrCycleCapabilityTest) {
  // Boilerplate to.get_device a corresponding to the bdf().
  auto raw_cfg = bus().ecam().get_config_view(bdf());
  auto* fake_dev = bus().ecam().get_device(bdf());

  // Two valid locations, followed by a third capability pointing at BAR 1.
  const uint8_t kCap1 = 0x80;
  const uint8_t kCap2 = 0x90;
  const uint8_t kCap3 = 0xA0;

  // Create a Cycle of Cap1 -> Cap2 -> Cap3 -> Cap1
  fake_dev->set_vendor_id(0x8086)
      .set_device_id(0x1234)
      .set_capabilities_list(1)
      .set_capabilities_ptr(kCap1);
  auto cap_id = static_cast<uint8_t>(Capability::Id::kVendor);
  raw_cfg.Write(cap_id, kCap1);
  raw_cfg.Write(kCap2, kCap1 + 1);
  raw_cfg.Write(cap_id, kCap2);
  raw_cfg.Write(kCap3, kCap2 + 1);
  raw_cfg.Write(cap_id, kCap3);
  raw_cfg.Write(kCap1, kCap3 + 1);

  auto cfg = bus().ecam().CreateMmioConfig(bdf());
  EXPECT_EQ(ZX_ERR_BAD_STATE, Device::Create(std::move(cfg), &upstream(), &bus(),
                                             /*has_acpi=*/false, /*has_devicetree=*/false));

  // Ensure no device was added.
  EXPECT_TRUE(bus().devices().empty());
}

// Test that we properly bail out if we see multiple of a capability
// type that only one should exist of in a system.
TEST_F(PciDeviceTestsCam, DuplicateFixedCapabilityTest) {
  // Boilerplate to.get_device a corresponding to the bdf().
  auto raw_cfg = bus().ecam().get_config_view(bdf());
  auto* fake_dev = bus().ecam().get_device(bdf());

  // Two valid locations, followed by a third capability pointing at BAR 1.
  const uint8_t kCap1 = 0x80;
  const uint8_t kCap2 = 0x90;
  const uint8_t kCap3 = 0xA0;

  // Create a device with three capabilities, two of which are kPciExpress
  fake_dev->set_vendor_id(0x8086)
      .set_device_id(0x1234)
      .set_capabilities_list(1)
      .set_capabilities_ptr(kCap1);
  auto pcie_id = static_cast<uint8_t>(Capability::Id::kPciExpress);
  auto null_id = static_cast<uint8_t>(Capability::Id::kNull);
  raw_cfg.Write8(pcie_id, kCap1);
  raw_cfg.Write8(kCap2, kCap1 + 1);
  raw_cfg.Write8(null_id, kCap2);
  raw_cfg.Write8(kCap3, kCap2 + 1);
  raw_cfg.Write8(pcie_id, kCap3);
  raw_cfg.Write8(0, kCap3 + 1);

  auto cfg = bus().ecam().CreateMmioConfig(bdf());
  EXPECT_EQ(ZX_ERR_BAD_STATE, Device::Create(std::move(cfg), &upstream(), &bus(),
                                             /*has_acpi=*/false, /*has_devicetree=*/false));

  // Ensure no device was added.
  EXPECT_TRUE(bus().devices().empty());
}

// Ensure we parse MSI capabilities properly in the Quadro device.
// lspci output: Capabilities: [68] MSI: Enable+ Count=1/4 Maskable- 64bit+
TEST_F(PciDeviceTestsExtendedCam, MsiCapabilityTest) {
  auto& dev = CreateTestDevice(kFakeQuadroDeviceConfig.data(), kFakeQuadroDeviceConfig.max_size());
  ASSERT_EQ(false, HasNonfatalFailure());
  ASSERT_NE(nullptr, dev.capabilities().msi);

  auto& msi = *dev.capabilities().msi;
  EXPECT_EQ(0x68u, msi.base());
  EXPECT_EQ(std::to_underlying(Capability::Id::kMsi), msi.id());
  EXPECT_EQ(true, msi.is_64bit());
  EXPECT_EQ(4u, msi.vectors_avail());
  EXPECT_EQ(false, msi.supports_pvm());

  // MSI should be disabled by Device initialization.
  auto ctrl = msi.ctrl().ReadFrom(dev.config().get());
  EXPECT_EQ(0, ctrl.enable());
}

// Ensure we parse MSIX capabilities properly in the Virtio-input device.
TEST_F(PciDeviceTestsExtendedCam, MsixCapabilityTest) {
  auto& dev = CreateTestDevice(kFakeVirtioInputDeviceConfig.data(),
                               kFakeVirtioInputDeviceConfig.max_size());
  ASSERT_EQ(false, HasNonfatalFailure());
  ASSERT_NE(nullptr, dev.capabilities().msix);

  auto& msix = *dev.capabilities().msix;
  EXPECT_EQ(0x98u, msix.base());
  EXPECT_EQ(std::to_underlying(Capability::Id::kMsiX), msix.id());
  EXPECT_EQ(1u, msix.table_bar());
  EXPECT_EQ(0u, msix.table_offset());
  EXPECT_EQ(2u, msix.table_size());
  EXPECT_EQ(1u, msix.pba_bar());
  EXPECT_EQ(0x800u, msix.pba_offset());

  // MSI-X should be disabled by Device initialization.
  auto ctrl = msix.ctrl().ReadFrom(dev.config().get());
  EXPECT_EQ(0u, ctrl.enable());
}

TEST_F(PciDeviceTestsExtendedCam, PciExpressCapabilityTest) {
  auto& dev = CreateTestDevice(kFakeQuadroDeviceConfig.data(), kFakeQuadroDeviceConfig.max_size());
  ASSERT_EQ(false, HasNonfatalFailure());
  ASSERT_NE(nullptr, dev.capabilities().pcie);

  const auto& pcie = *dev.capabilities().pcie;
  EXPECT_EQ(0x78u, pcie.base());
  EXPECT_EQ(std::to_underlying(Capability::Id::kPciExpress), pcie.id());
}

TEST_F(PciDeviceTestsExtendedCam, MapInterruptMsi) {
  pci::Device& dev =
      CreateTestDevice(kFakeQuadroDeviceConfig.data(), kFakeQuadroDeviceConfig.max_size());
  const auto mode = fuchsia_hardware_pci::InterruptMode::kMsi;
  ASSERT_OK(dev.SetIrqMode(mode, 1));
  ExpectMsiState(dev, true);

  auto result = dev.MapInterrupt(0);
  ASSERT_TRUE(result.is_ok());
  EXPECT_TRUE(result.value().is_valid());
  // Disabling MSI while an interrupt handle is still held should fail with ZX_ERR_BAD_STATE.
  EXPECT_STATUS(ZX_ERR_BAD_STATE,
                dev.SetIrqMode(fuchsia_hardware_pci::InterruptMode::kDisabled, 0));
  result.value().reset();
  EXPECT_OK(dev.SetIrqMode(fuchsia_hardware_pci::InterruptMode::kDisabled, 0));
  ExpectMsiState(dev, false);
}

TEST_F(PciDeviceTestsExtendedCam, MapInterruptMsiX) {
  pci::Device& dev =
      CreateTestDevice(kFakeQuadroDeviceConfig.data(), kFakeQuadroDeviceConfig.max_size());
  ConfigureDownstreamDevices();
  const auto mode = fuchsia_hardware_pci::InterruptMode::kMsiX;
  ASSERT_OK(dev.SetIrqMode(mode, 1));
  ExpectMsixCtrl(dev, /*enabled=*/true, /*function_mask=*/true);

  auto result = dev.MapInterrupt(0);
  ASSERT_TRUE(result.is_ok());
  EXPECT_TRUE(result.value().is_valid());
  ExpectMsixCtrl(dev, /*enabled=*/true, /*function_mask=*/false);
  EXPECT_STATUS(ZX_ERR_BAD_STATE,
                dev.SetIrqMode(fuchsia_hardware_pci::InterruptMode::kDisabled, 0));
  result.value().reset();
  EXPECT_OK(dev.SetIrqMode(fuchsia_hardware_pci::InterruptMode::kDisabled, 0));
  ExpectMsixCtrl(dev, /*enabled=*/false, /*function_mask=*/true);
}

TEST_F(PciDeviceTestsExtendedCam, NonKernelMsiAllocationDisableAndReenable) {
  bus().set_use_non_kernel_msi_allocation(true);
  pci::Device& dev =
      CreateTestDevice(kFakeQuadroDeviceConfig.data(), kFakeQuadroDeviceConfig.max_size());
  ConfigureDownstreamDevices();

  // Enable MSI, map an interrupt, disable MSI, and re-enable MSI (as happens across endpoint
  // driver restarts on platforms like google-pci where PcirootAllocateMsi returns a placeholder
  // zx::event).
  ASSERT_NE(dev.capabilities().msi, nullptr);
  auto& msi = *dev.capabilities().msi;
  // Write non-zero to tgt_addr_upper to simulate stale firmware state.
  msi.tgt_addr_upper().FromValue(0xA5A5A5A5u).WriteTo(dev.config().get());
  ASSERT_OK(dev.SetIrqMode(fuchsia_hardware_pci::InterruptMode::kMsi, 1));
  ExpectMsiState(dev, true);
  {
    EXPECT_EQ(FakeBus::kDefaultNonKernelMsiTargetAddr,
              msi.tgt_addr().ReadFrom(dev.config().get()).reg_value());
    EXPECT_EQ(0u, msi.tgt_addr_upper().ReadFrom(dev.config().get()).reg_value());
    auto irq = dev.MapInterrupt(0);
    ASSERT_OK(irq.status_value());
    EXPECT_TRUE(irq->is_valid());
  }
  ASSERT_OK(dev.SetIrqMode(fuchsia_hardware_pci::InterruptMode::kDisabled, 0));
  ExpectMsiState(dev, false);
  ASSERT_OK(dev.SetIrqMode(fuchsia_hardware_pci::InterruptMode::kMsi, 1));
  ExpectMsiState(dev, true);
  {
    auto irq = dev.MapInterrupt(0);
    ASSERT_OK(irq.status_value());
    EXPECT_TRUE(irq->is_valid());
  }

  // Also verify MSI-X disable and re-enable with a non-kernel MSI allocation.
  ASSERT_OK(dev.SetIrqMode(fuchsia_hardware_pci::InterruptMode::kMsiX, 1));
  ExpectMsixCtrl(dev, /*enabled=*/true, /*function_mask=*/true);
  {
    auto irq = dev.MapInterrupt(0);
    ASSERT_OK(irq.status_value());
    EXPECT_TRUE(irq->is_valid());
    ExpectMsixCtrl(dev, /*enabled=*/true, /*function_mask=*/false);
  }
  ASSERT_OK(dev.SetIrqMode(fuchsia_hardware_pci::InterruptMode::kDisabled, 0));
  ExpectMsixCtrl(dev, /*enabled=*/false, /*function_mask=*/true);
}

// Verify that power state transitions wait the necessary amount of time, and that they end up in
// the correct state.
TEST_F(PciDeviceTestsExtendedCam, PowerStateTransitions) {
  pci::Device& dev =
      CreateTestDevice(kFakeQuadroDeviceConfig.data(), kFakeQuadroDeviceConfig.max_size());
  pci::Config& cfg = *dev.config();

  auto power = PowerManagementCapability(*dev.config(), kFakeQuadroPowerManagementCapabilityOffset);
  auto test_recovery_delay = [&cfg, &power](
                                 PowerManagementCapability::PowerState start_state,
                                 PowerManagementCapability::PowerState end_state) -> bool {
    // Manually update our starting state
    power.pmcsr().ReadFrom(&cfg).set_power_state(start_state).WriteTo(&cfg);
    // Time the transition
    const zx::time start_time = zx::clock::get_monotonic();
    power.SetPowerState(cfg, end_state);
    const zx::time end_time = zx::clock::get_monotonic();
    const zx::duration min_delay =
        PowerManagementCapability::kStateRecoveryTime[start_state][end_state];
    return (end_time - start_time > min_delay);
  };

  ASSERT_TRUE(test_recovery_delay(PowerManagementCapability::PowerState::D0,
                                  PowerManagementCapability::PowerState::D1));
  ASSERT_EQ(dev.GetPowerState().value(), PowerManagementCapability::PowerState::D1);

  ASSERT_TRUE(test_recovery_delay(PowerManagementCapability::PowerState::D0,
                                  PowerManagementCapability::PowerState::D2));
  ASSERT_EQ(dev.GetPowerState().value(), PowerManagementCapability::PowerState::D2);

  ASSERT_TRUE(test_recovery_delay(PowerManagementCapability::PowerState::D0,
                                  PowerManagementCapability::PowerState::D3));
  ASSERT_EQ(dev.GetPowerState().value(), PowerManagementCapability::PowerState::D3);

  ASSERT_TRUE(test_recovery_delay(PowerManagementCapability::PowerState::D3,
                                  PowerManagementCapability::PowerState::D0));
  ASSERT_EQ(dev.GetPowerState().value(), PowerManagementCapability::PowerState::D0);

  // D0 to D0 should be essentially a no-op return.
  ASSERT_TRUE(test_recovery_delay(PowerManagementCapability::PowerState::D0,
                                  PowerManagementCapability::PowerState::D0));
  ASSERT_EQ(dev.GetPowerState().value(), PowerManagementCapability::PowerState::D0);

  // D1 to D2 should actually run D1 > D0 > D2 and hit both code paths.
  ASSERT_TRUE(test_recovery_delay(PowerManagementCapability::PowerState::D2,
                                  PowerManagementCapability::PowerState::D1));
  ASSERT_EQ(dev.GetPowerState().value(), PowerManagementCapability::PowerState::D1);
}

// Verify that power state transitions don't write PME_Status back as 1, which would clear a
// pending PME. The fake config space stores writes as-is, so a cleared bit shows the value written.
TEST_F(PciDeviceTestsExtendedCam, PowerStatePreservesPmeStatus) {
  pci::Device& dev =
      CreateTestDevice(kFakeQuadroDeviceConfig.data(), kFakeQuadroDeviceConfig.max_size());
  pci::Config& cfg = *dev.config();

  auto power = PowerManagementCapability(cfg, kFakeQuadroPowerManagementCapabilityOffset);
  power.pmcsr()
      .ReadFrom(&cfg)
      .set_power_state(PowerManagementCapability::PowerState::D0)
      .set_pme_status(1)
      .WriteTo(&cfg);
  power.SetPowerState(cfg, PowerManagementCapability::PowerState::D3);

  auto pmcsr = power.pmcsr().ReadFrom(&cfg);
  EXPECT_EQ(pmcsr.power_state(), PowerManagementCapability::PowerState::D3);
  EXPECT_EQ(pmcsr.pme_status(), 0u);
}

// Verify that a prefetchable BAR behind a bridge falls back to the non-prefetchable
// MMIO window when PF-MMIO allocation fails.
TEST_F(PciDeviceTestExtendedCamBridge, PrefetchableBridgeBarMmioFallback) {
  [[maybe_unused]] auto& dev =
      CreateTestDevice(kFakeQuadroDeviceConfig.data(), kFakeQuadroDeviceConfig.max_size());

  // BAR 0 of |kFakeQuadroDeviceConfig| is non-prefetchable and allocates from the regular MMIO
  // window. BAR 1 of |kFakeQuadroDeviceConfig| is the first prefetchable BAR, so it will be the
  // first to attempt allocation from the PF-MMIO window.

  // Fail the next prefetchable allocation (BAR 1) in order to trigger the fallback to the
  // non-prefetchable MMIO window of the bridge.
  auto& pf_alloc = upstream().fake_pf_mmio_regions();
  auto& mmio_alloc = upstream().fake_mmio_regions();
  pf_alloc.FailNextAllocation(/*assigned_only=*/false);
  ConfigureDownstreamDevices();

  // BAR 1 fails the prefetchable allocation.
  const auto& pf_log = pf_alloc.allocation_log();
  EXPECT_TRUE(std::ranges::any_of(pf_log, [](const AllocationLogEntry& e) {
    return e.size == kTestDeviceBars[1].size && !e.succeeded;
  }));
  // BAR 1 successfully falls back to the non-prefetchable allocation.
  const auto& mmio_log = mmio_alloc.allocation_log();
  EXPECT_TRUE(std::ranges::any_of(mmio_log, [](const AllocationLogEntry& e) {
    return e.size == kTestDeviceBars[1].size && e.succeeded;
  }));
}

TEST_F(PciDeviceTestsExtendedCam, MsixInvalidBir) {
  // Case 1: Table BIR exceeds BAR count.
  {
    auto cfg_buf = kFakeVirtioInputDeviceConfig;
    cfg_buf[kFakeVirtioInputMsixCapabilityOffset + MsixCapability::kMsixTableRegisterOffset] =
        kMaxBarCount;
    pci::Device& dev = CreateTestDevice(cfg_buf.data(), cfg_buf.max_size());
    ConfigureDownstreamDevices();
    {
      fbl::AutoLock lock(dev.dev_lock());
      EXPECT_TRUE(dev.disabled());
    }
    upstream().DisableDownstream();
    upstream().UnplugDownstream();
  }

  // Case 2: PBA BIR exceeds BAR count.
  {
    auto cfg_buf = kFakeVirtioInputDeviceConfig;
    cfg_buf[kFakeVirtioInputMsixCapabilityOffset + MsixCapability::kMsixPbaRegisterOffset] =
        kMaxBarCount;
    pci::Device& dev = CreateTestDevice(cfg_buf.data(), cfg_buf.max_size());
    ConfigureDownstreamDevices();
    {
      fbl::AutoLock lock(dev.dev_lock());
      EXPECT_TRUE(dev.disabled());
    }
    upstream().DisableDownstream();
    upstream().UnplugDownstream();
  }

  // Case 3: Table BIR names an unimplemented BAR (BAR 0 in virtio-input config is unused).
  {
    auto cfg_buf = kFakeVirtioInputDeviceConfig;
    cfg_buf[kFakeVirtioInputMsixCapabilityOffset + MsixCapability::kMsixTableRegisterOffset] = 0;
    pci::Device& dev = CreateTestDevice(cfg_buf.data(), cfg_buf.max_size());
    ConfigureDownstreamDevices();
    {
      fbl::AutoLock lock(dev.dev_lock());
      EXPECT_TRUE(dev.disabled());
    }
    upstream().DisableDownstream();
    upstream().UnplugDownstream();
  }
}

TEST_F(PciDeviceTestsExtendedCam, Msi32BitAddressLimit) {
  constexpr uint64_t kAbove4GiBAddress = 0x1'0000'1000ULL;

  // Case 1: Bus mastering disabled initially; stays disabled after rejection.
  {
    auto& ecam = bus().ecam();
    ecam.get_config_view(bdf()).WriteBuffer(0, kFakeQuadroDeviceConfig.data(),
                                            kFakeQuadroDeviceConfig.max_size());
    auto mmio_cfg = ecam.CreateMmioConfig(bdf());
    MsiCapability msi(*mmio_cfg, kFakeQuadroMsiCapabilityOffset);
    msi.ctrl().ReadFrom(mmio_cfg.get()).set_is_64bit_capable(0).WriteTo(mmio_cfg.get());

    bus().set_use_non_kernel_msi_allocation(true, kAbove4GiBAddress);
    pci::Device& dev = CreateTestDevice();
    ConfigureDownstreamDevices();

    {
      fbl::AutoLock lock(dev.dev_lock());
      ASSERT_OK(dev.SetBusMastering(false));
    }
    EXPECT_FALSE(config::Command.ReadFrom(dev.config().get()).bus_master());

    EXPECT_EQ(dev.SetIrqMode(fuchsia_hardware_pci::InterruptMode::kMsi, 1), ZX_ERR_NOT_SUPPORTED);
    EXPECT_FALSE(config::Command.ReadFrom(dev.config().get()).bus_master());
    upstream().DisableDownstream();
    upstream().UnplugDownstream();
  }

  // Case 2: Bus mastering enabled initially; stays enabled after rejection.
  {
    auto& ecam = bus().ecam();
    ecam.get_config_view(bdf()).WriteBuffer(0, kFakeQuadroDeviceConfig.data(),
                                            kFakeQuadroDeviceConfig.max_size());
    auto mmio_cfg = ecam.CreateMmioConfig(bdf());
    MsiCapability msi(*mmio_cfg, kFakeQuadroMsiCapabilityOffset);
    msi.ctrl().ReadFrom(mmio_cfg.get()).set_is_64bit_capable(0).WriteTo(mmio_cfg.get());

    bus().set_use_non_kernel_msi_allocation(true, kAbove4GiBAddress);
    pci::Device& dev = CreateTestDevice();
    ConfigureDownstreamDevices();

    {
      fbl::AutoLock lock(dev.dev_lock());
      ASSERT_OK(dev.SetBusMastering(true));
    }
    EXPECT_TRUE(config::Command.ReadFrom(dev.config().get()).bus_master());

    EXPECT_EQ(dev.SetIrqMode(fuchsia_hardware_pci::InterruptMode::kMsi, 1), ZX_ERR_NOT_SUPPORTED);
    EXPECT_TRUE(config::Command.ReadFrom(dev.config().get()).bus_master());
    upstream().DisableDownstream();
    upstream().UnplugDownstream();
  }
}

TEST_F(PciDeviceTestsExtendedCam, BridgeWindowDecode) {
  auto& ecam = bus().ecam();
  constexpr fuchsia_hardware_pci::wire::Bdf kBridgeBdf = {
      .bus_id = 0, .device_id = 1, .function_id = 0};
  ecam.get_bridge(kBridgeBdf)
      ->set_vendor_id(0x8086)
      .set_device_id(1)
      .set_header_type(kHeaderTypePciBridge)
      .set_io_base(0x11)
      .set_io_limit(0xF1)
      .set_io_base_upper(0x0002)
      .set_io_limit_upper(0x0004)
      .set_memory_base(0x1000)
      .set_memory_limit(0xBFFF)
      .set_prefetchable_memory_base(0x2001)
      .set_prefetchable_memory_limit(0x3FF1)
      .set_prfetchable_memory_base_upper(0x00000001)
      .set_prfetchable_memory_limit_upper(0x00000002)
      .set_primary_bus_number(0)
      .set_secondary_bus_number(1)
      .set_subordinate_bus_number(1);
  auto bridge_mmio_cfg = ecam.CreateMmioConfig(kBridgeBdf);
  auto bridge_view = bridge_mmio_cfg->get_view();
  ASSERT_OK(bridge_view.status_value());
  auto bridge_cfg = std::make_unique<FakeMmioConfig>(kBridgeBdf, std::move(bridge_view.value()));
  fbl::RefPtr<pci::Bridge> bridge;
  ASSERT_OK(Bridge::Create(std::move(bridge_cfg), &upstream(), &bus(), 1, &bridge));

  EXPECT_EQ(bridge->io_base(), 0x00021000u);
  EXPECT_EQ(bridge->io_limit(), 0x0004FFFFu);
  EXPECT_EQ(bridge->mem_base(), 0x10000000u);
  EXPECT_EQ(bridge->mem_limit(), 0xBFFFFFFFu);
  EXPECT_EQ(bridge->pf_mem_base(), 0x0000000120000000ULL);
  EXPECT_EQ(bridge->pf_mem_limit(), 0x000000023FFFFFFFULL);

  ConfigureDownstreamDevices();
  EXPECT_TRUE(config::Command.ReadFrom(bridge->config().get()).io_space());
  EXPECT_TRUE(config::Command.ReadFrom(bridge->config().get()).memory_space());
}

TEST_F(PciDeviceTestsExtendedCam, CommandRegister) {
  pci::Device& dev =
      CreateTestDevice(kFakeQuadroDeviceConfig.data(), kFakeQuadroDeviceConfig.max_size());
  EXPECT_TRUE(config::Command.ReadFrom(dev.config().get()).interrupt_disable());

  ConfigureDownstreamDevices();
  auto cmd = config::Command.ReadFrom(dev.config().get());
  EXPECT_TRUE(cmd.memory_space());
  EXPECT_TRUE(cmd.io_space());
  EXPECT_TRUE(cmd.interrupt_disable());

  {
    fbl::AutoLock lock(dev.dev_lock());
    ASSERT_OK(dev.SetBusMastering(true));
  }
  cmd = config::Command.ReadFrom(dev.config().get());
  EXPECT_TRUE(cmd.bus_master());
  EXPECT_TRUE(cmd.memory_space());
  EXPECT_TRUE(cmd.io_space());
  EXPECT_TRUE(cmd.interrupt_disable());

  {
    fbl::AutoLock lock(dev.dev_lock());
    ASSERT_OK(dev.SetBusMastering(false));
  }
  cmd = config::Command.ReadFrom(dev.config().get());
  EXPECT_FALSE(cmd.bus_master());
  EXPECT_TRUE(cmd.memory_space());
  EXPECT_TRUE(cmd.io_space());
  EXPECT_TRUE(cmd.interrupt_disable());

  ASSERT_OK(dev.SetIrqMode(fuchsia_hardware_pci::InterruptMode::kLegacy, 1));
  EXPECT_FALSE(config::Command.ReadFrom(dev.config().get()).interrupt_disable());
  ASSERT_OK(dev.SetIrqMode(fuchsia_hardware_pci::InterruptMode::kDisabled, 0));
  EXPECT_TRUE(config::Command.ReadFrom(dev.config().get()).interrupt_disable());

  upstream().DisableDownstream();
  cmd = config::Command.ReadFrom(dev.config().get());
  EXPECT_FALSE(cmd.io_space());
  EXPECT_FALSE(cmd.memory_space());
  EXPECT_FALSE(cmd.bus_master());
  EXPECT_TRUE(cmd.interrupt_disable());
}

}  // namespace
}  // namespace pci
