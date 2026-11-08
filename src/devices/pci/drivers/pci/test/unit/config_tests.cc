// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.pci.root/cpp/driver/wire.h>
#include <fidl/fuchsia.hardware.pci.root/cpp/fidl.h>
#include <lib/driver/testing/cpp/driver_runtime.h>
#include <lib/driver/testing/cpp/scoped_global_logger.h>
#include <lib/zx/result.h>
#include <zircon/limits.h>

#include <memory>

#include <gtest/gtest.h>

#include "src/devices/pci/drivers/pci/config.h"
#include "src/devices/pci/drivers/pci/test/fakes/fake_root_service.h"
#include "src/devices/pci/lib/cpp/capabilities.h"
#include "src/lib/testing/predicates/status.h"

namespace pci {
namespace {

constexpr fuchsia_hardware_pci::wire::Bdf kDefaultBdf1 = {
    .bus_id = 0,
    .device_id = 1,
    .function_id = 2,
};
constexpr fuchsia_hardware_pci::wire::Bdf kDefaultBdf2 = {
    .bus_id = 1,
    .device_id = 2,
    .function_id = 3,
};

template <bool IsExtended>
class PciConfigTests : public ::testing::Test {
 public:
  FakeEcam& ecam() { return ecam_; }
  const fdf::WireSyncClient<fuchsia_hardware_pci_root::Config>& config_client() {
    return *config_client_;
  }

 protected:
  void SetUp() final {
    auto [client_end, server_end] = fdf::Endpoints<fuchsia_hardware_pci_root::Config>::Create();
    config_server_.Bind(server_dispatcher_->get(), std::move(server_end));
    config_client_ = std::make_unique<fdf::WireSyncClient<fuchsia_hardware_pci_root::Config>>(
        std::move(client_end));
  }

  void TearDown() final { config_client_.reset(); }

 private:
  fdf_testing::DriverRuntime runtime_;
  fdf_testing::ScopedGlobalLogger logger_;
  fdf::UnownedSynchronizedDispatcher server_dispatcher_ = runtime_.StartBackgroundDispatcher();
  FakeEcam ecam_{/*bus_start=*/0, /*bus_cnt=*/2, IsExtended};
  FakeConfigServer config_server_{ecam_};
  std::unique_ptr<fdf::WireSyncClient<fuchsia_hardware_pci_root::Config>> config_client_;
};

using PciConfigBaseTests = PciConfigTests<false>;
using PciConfigExtendedTests = PciConfigTests<true>;

void DeviceConfigIntegrationImpl(FakeEcam& ecam) {
  FakePciType0Config* dev1 = ecam.get_device(kDefaultBdf1);
  dev1->set_vendor_id(0x8086)
      .set_device_id(0x1234)
      .set_header_type(0x01)
      .set_revision_id(12)
      .set_expansion_rom_address(0xFF0000EE);
  // Test 8, 16, and 32 bit reads.
  auto cfg1 = ecam.CreateMmioConfig(kDefaultBdf1);
  EXPECT_EQ(dev1->revision_id(), config::RevisionId.ReadFrom(cfg1.get()).reg_value());
  EXPECT_EQ(dev1->vendor_id(), config::VendorId.ReadFrom(cfg1.get()).reg_value());
  EXPECT_EQ(dev1->device_id(), config::DeviceId.ReadFrom(cfg1.get()).reg_value());
  EXPECT_EQ(dev1->header_type(), config::HeaderType.ReadFrom(cfg1.get()).reg_value());
  EXPECT_EQ(dev1->expansion_rom_address(),
            config::ExpansionRomAddress.ReadFrom(cfg1.get()).reg_value());
  // Now try the same thing for a different, unconfigured device and ensure they aren't
  // overlapping somehow.
  auto cfg2 = ecam.CreateMmioConfig(kDefaultBdf2);
  EXPECT_EQ(0x0u, config::RevisionId.ReadFrom(cfg2.get()).reg_value());
  EXPECT_EQ(0xFFFFu, config::VendorId.ReadFrom(cfg2.get()).reg_value());
  EXPECT_EQ(0xFFFFu, config::DeviceId.ReadFrom(cfg2.get()).reg_value());
  EXPECT_EQ(0x0u, config::HeaderType.ReadFrom(cfg2.get()).reg_value());
  EXPECT_EQ(0x0u, config::ExpansionRomAddress.ReadFrom(cfg2.get()).reg_value());

  FakePciType0Config* dev2 = ecam.get_device(kDefaultBdf2);
  dev2->set_vendor_id(0x8680)
      .set_device_id(0x4321)
      .set_header_type(0x02)
      .set_revision_id(3)
      .set_expansion_rom_address(0xEE0000FF);

  EXPECT_EQ(dev2->revision_id(), config::RevisionId.ReadFrom(cfg2.get()).reg_value());
  EXPECT_EQ(dev2->vendor_id(), config::VendorId.ReadFrom(cfg2.get()).reg_value());
  EXPECT_EQ(dev2->device_id(), config::DeviceId.ReadFrom(cfg2.get()).reg_value());
  EXPECT_EQ(dev2->header_type(), config::HeaderType.ReadFrom(cfg2.get()).reg_value());
  EXPECT_EQ(dev2->expansion_rom_address(),
            config::ExpansionRomAddress.ReadFrom(cfg2.get()).reg_value());
}

void ConfirmDevConfigReadResetImpl(pci::Config* cfg, FakePciType0Config* dev) {
  ASSERT_EQ(0xFFFFu, dev->vendor_id());
  ASSERT_EQ(0xFFFFu, dev->device_id());
  ASSERT_EQ(0x0u, dev->command());
  ASSERT_EQ(0x0u, dev->status());
  ASSERT_EQ(0x0u, dev->revision_id());
  ASSERT_EQ(0x0u, dev->program_interface());
  ASSERT_EQ(0x0u, dev->sub_class());
  ASSERT_EQ(0x0u, dev->base_class());
  ASSERT_EQ(0x0u, dev->cache_line_size());
  ASSERT_EQ(0x0u, dev->latency_timer());
  ASSERT_EQ(0x0u, dev->header_type());
  ASSERT_EQ(0x0u, dev->bist());
  ASSERT_EQ(0x0u, dev->cardbus_cis_ptr());
  ASSERT_EQ(0x0u, dev->subsystem_vendor_id());
  ASSERT_EQ(0x0u, dev->subsystem_id());
  ASSERT_EQ(0x0u, dev->expansion_rom_address());
  ASSERT_EQ(0x0u, dev->capabilities_ptr());
  ASSERT_EQ(0x0u, dev->interrupt_line());
  ASSERT_EQ(0x0u, dev->interrupt_pin());
  ASSERT_EQ(0x0u, dev->min_grant());
  ASSERT_EQ(0x0u, dev->max_latency());

  // Ensure the config header reads match the reset values above, this time
  // through the config interface.
  EXPECT_EQ(0xFFFFu, config::VendorId.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0xFFFFu, config::DeviceId.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::Command.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::Status.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::RevisionId.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::ProgramInterface.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::SubClass.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::BaseClass.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::CacheLineSize.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::LatencyTimer.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::HeaderType.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::Bist.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::CardbusCisPtr.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::SubsystemVendorId.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::SubsystemId.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::ExpansionRomAddress.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::CapabilitiesPtr.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::InterruptLine.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::InterruptPin.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::MinGrant.ReadFrom(cfg).reg_value());
  EXPECT_EQ(0x0u, config::MaxLatency.ReadFrom(cfg).reg_value());
}

// This tests verifies the read/write interface for config works with both the actual config type,
// and the test based PciType0Config type.
void ConfigReadWriteIntegrationImpl(pci::Config* cfg, FakePciType0Config* dev) {
  config::VendorId.FromValue(0x1234).WriteTo(cfg);
  EXPECT_EQ(0x1234u, dev->vendor_id());
  EXPECT_EQ(0x1234u, config::VendorId.ReadFrom(cfg).reg_value());

  // Write test data to the config header registers.
  uint32_t value = 0x00;
  config::VendorId.FromValue(static_cast<uint16_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->vendor_id());
  EXPECT_EQ(value, config::VendorId.ReadFrom(cfg).reg_value());

  config::DeviceId.FromValue(static_cast<uint16_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->device_id());
  EXPECT_EQ(value, config::DeviceId.ReadFrom(cfg).reg_value());

  config::Command.FromValue(static_cast<uint16_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->command());
  EXPECT_EQ(value, config::Command.ReadFrom(cfg).reg_value());

  config::Status.FromValue(static_cast<uint16_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->status());
  EXPECT_EQ(value, config::Status.ReadFrom(cfg).reg_value());

  config::RevisionId.FromValue(static_cast<uint8_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->revision_id());
  EXPECT_EQ(value, config::RevisionId.ReadFrom(cfg).reg_value());

  config::ProgramInterface.FromValue(static_cast<uint8_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->program_interface());
  EXPECT_EQ(value, config::ProgramInterface.ReadFrom(cfg).reg_value());

  config::SubClass.FromValue(static_cast<uint8_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->sub_class());
  EXPECT_EQ(value, config::SubClass.ReadFrom(cfg).reg_value());

  config::BaseClass.FromValue(static_cast<uint8_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->base_class());
  EXPECT_EQ(value, config::BaseClass.ReadFrom(cfg).reg_value());

  config::CacheLineSize.FromValue(static_cast<uint8_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->cache_line_size());
  EXPECT_EQ(value, config::CacheLineSize.ReadFrom(cfg).reg_value());

  config::LatencyTimer.FromValue(static_cast<uint8_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->latency_timer());
  EXPECT_EQ(value, config::LatencyTimer.ReadFrom(cfg).reg_value());

  config::HeaderType.FromValue(static_cast<uint8_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->header_type());
  EXPECT_EQ(value, config::HeaderType.ReadFrom(cfg).reg_value());

  config::Bist.FromValue(static_cast<uint8_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->bist());
  EXPECT_EQ(value, config::Bist.ReadFrom(cfg).reg_value());

  config::CardbusCisPtr.FromValue(++value).WriteTo(cfg);
  EXPECT_EQ(value, dev->cardbus_cis_ptr());
  EXPECT_EQ(value, config::CardbusCisPtr.ReadFrom(cfg).reg_value());

  config::SubsystemVendorId.FromValue(static_cast<uint16_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->subsystem_vendor_id());
  EXPECT_EQ(value, config::SubsystemVendorId.ReadFrom(cfg).reg_value());

  config::SubsystemId.FromValue(static_cast<uint16_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->subsystem_id());
  EXPECT_EQ(value, config::SubsystemId.ReadFrom(cfg).reg_value());

  config::ExpansionRomAddress.FromValue(++value).WriteTo(cfg);
  EXPECT_EQ(value, dev->expansion_rom_address());
  EXPECT_EQ(value, config::ExpansionRomAddress.ReadFrom(cfg).reg_value());

  config::CapabilitiesPtr.FromValue(static_cast<uint8_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->capabilities_ptr());
  EXPECT_EQ(value, config::CapabilitiesPtr.ReadFrom(cfg).reg_value());

  config::InterruptLine.FromValue(static_cast<uint8_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->interrupt_line());
  EXPECT_EQ(value, config::InterruptLine.ReadFrom(cfg).reg_value());

  config::InterruptPin.FromValue(static_cast<uint8_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->interrupt_pin());
  EXPECT_EQ(value, config::InterruptPin.ReadFrom(cfg).reg_value());

  config::MinGrant.FromValue(static_cast<uint8_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->min_grant());
  EXPECT_EQ(value, config::MinGrant.ReadFrom(cfg).reg_value());

  config::MaxLatency.FromValue(static_cast<uint8_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, dev->max_latency());
  EXPECT_EQ(value, config::MaxLatency.ReadFrom(cfg).reg_value());

  cfg->Write<uint8_t>(0xA5, static_cast<uint16_t>(config::RevisionId.addr()));
  EXPECT_EQ(0xA5u, dev->revision_id());
  EXPECT_EQ(0xA5u, cfg->Read<uint8_t>(static_cast<uint16_t>(config::RevisionId.addr())));

  cfg->Write<uint16_t>(0x5A5A, static_cast<uint16_t>(config::SubsystemId.addr()));
  EXPECT_EQ(0x5A5Au, dev->subsystem_id());
  EXPECT_EQ(0x5A5Au, cfg->Read<uint16_t>(static_cast<uint16_t>(config::SubsystemId.addr())));

  cfg->Write<uint32_t>(0x12345678, static_cast<uint16_t>(config::ExpansionRomAddress.addr()));
  EXPECT_EQ(0x12345678u, dev->expansion_rom_address());
  EXPECT_EQ(0x12345678u,
            cfg->Read<uint32_t>(static_cast<uint16_t>(config::ExpansionRomAddress.addr())));

  config::Command.FromValue(0).set_io_space(1).set_memory_space(1).set_bus_master(1).WriteTo(cfg);
  EXPECT_EQ(0x0007u, dev->command());
  auto command = config::Command.ReadFrom(cfg);
  EXPECT_TRUE(command.io_space());
  EXPECT_TRUE(command.memory_space());
  EXPECT_TRUE(command.bus_master());
  EXPECT_FALSE(command.interrupt_disable());
  EXPECT_EQ(command.reg_value(), dev->command());

  config::Command.FromValue(0).set_interrupt_disable(1).WriteTo(cfg);
  EXPECT_EQ(0x0400u, dev->command());
  EXPECT_TRUE(config::Command.ReadFrom(cfg).interrupt_disable());

  config::Status.FromValue(0).set_has_capabilities_list(1).set_fast_b2b_capable(1).WriteTo(cfg);
  EXPECT_EQ(0x0090u, dev->status());
  auto status = config::Status.ReadFrom(cfg);
  EXPECT_TRUE(status.has_capabilities_list());
  EXPECT_TRUE(status.fast_b2b_capable());
  EXPECT_FALSE(status.interrupt_status());
  EXPECT_EQ(status.reg_value(), dev->status());

  config::BaseAddress(0)
      .FromValue(0)
      .mmio()
      .set_base_address(0xABCD0000)
      .set_is_prefetchable(1)
      .set_type(config::MmioBaseAddressReg::kType64Bit)
      .WriteTo(cfg);
  auto read_bar = config::BaseAddress(0).ReadFrom(cfg);
  EXPECT_FALSE(read_bar.is_io_space());
  EXPECT_TRUE(read_bar.mmio().is_prefetchable());
  EXPECT_EQ(read_bar.mmio().type(), config::MmioBaseAddressReg::kType64Bit);
  EXPECT_EQ(0xABCD0000u, read_bar.mmio().base_address());
  EXPECT_EQ(read_bar.reg_value(), dev->base_address[0].mmio64);

  config::BaseAddress(1).FromValue(0).io().set_is_io_space(1).set_base_address(0xFF00).WriteTo(cfg);
  auto read_io_bar = config::BaseAddress(1).ReadFrom(cfg);
  EXPECT_TRUE(read_io_bar.is_io_space());
  EXPECT_EQ(0xFF00u, read_io_bar.io().base_address());
  EXPECT_EQ(read_io_bar.reg_value(), dev->base_address[1].io.value);
  EXPECT_TRUE(dev->base_address[1].io.is_io_space());
  EXPECT_EQ(0xFF00u >> 2u, dev->base_address[1].io.address());
}

void BridgeConfigReadWriteIntegrationImpl(pci::Config* cfg, FakePciType1Config* bridge) {
  uint32_t value = 0x10;

  config::PrimaryBusId.FromValue(static_cast<uint8_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, bridge->primary_bus_number());
  EXPECT_EQ(value, config::PrimaryBusId.ReadFrom(cfg).reg_value());

  config::SecondaryBusId.FromValue(static_cast<uint8_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, bridge->secondary_bus_number());
  EXPECT_EQ(value, config::SecondaryBusId.ReadFrom(cfg).reg_value());

  config::SubordinateBusId.FromValue(static_cast<uint8_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, bridge->subordinate_bus_number());
  EXPECT_EQ(value, config::SubordinateBusId.ReadFrom(cfg).reg_value());

  config::SecondaryLatencyTimer.FromValue(static_cast<uint8_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, bridge->secondary_latency_timer());
  EXPECT_EQ(value, config::SecondaryLatencyTimer.ReadFrom(cfg).reg_value());

  config::IoBase.FromValue(0).set_address(0xA).set_address_type(0x1).WriteTo(cfg);
  EXPECT_EQ(0xA1u, bridge->io_base());
  auto io_base = config::IoBase.ReadFrom(cfg);
  EXPECT_EQ(0xAu, io_base.address());
  EXPECT_EQ(0x1u, io_base.address_type());

  config::IoLimit.FromValue(0).set_address(0xB).set_address_type(0x1).WriteTo(cfg);
  EXPECT_EQ(0xB1u, bridge->io_limit());
  auto io_limit = config::IoLimit.ReadFrom(cfg);
  EXPECT_EQ(0xBu, io_limit.address());
  EXPECT_EQ(0x1u, io_limit.address_type());

  config::SecondaryStatus.FromValue(static_cast<uint16_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, bridge->secondary_status());
  EXPECT_EQ(value, config::SecondaryStatus.ReadFrom(cfg).reg_value());

  config::MemoryBase.FromValue(0).set_address(0x123).WriteTo(cfg);
  EXPECT_EQ(0x1230u, bridge->memory_base());
  auto mem_base = config::MemoryBase.ReadFrom(cfg);
  EXPECT_EQ(0x123u, mem_base.address());

  config::MemoryLimit.FromValue(0).set_address(0x456).WriteTo(cfg);
  EXPECT_EQ(0x4560u, bridge->memory_limit());
  auto mem_limit = config::MemoryLimit.ReadFrom(cfg);
  EXPECT_EQ(0x456u, mem_limit.address());

  config::PrefetchableMemoryBase.FromValue(0).set_address(0x789).set_address_type(0x1).WriteTo(cfg);
  EXPECT_EQ(0x7891u, bridge->prefetchable_memory_base());
  auto pf_base = config::PrefetchableMemoryBase.ReadFrom(cfg);
  EXPECT_EQ(0x789u, pf_base.address());
  EXPECT_EQ(0x1u, pf_base.address_type());

  config::PrefetchableMemoryLimit.FromValue(0).set_address(0xABC).set_address_type(0x1).WriteTo(
      cfg);
  EXPECT_EQ(0xABC1u, bridge->prefetchable_memory_limit());
  auto pf_limit = config::PrefetchableMemoryLimit.ReadFrom(cfg);
  EXPECT_EQ(0xABCu, pf_limit.address());
  EXPECT_EQ(0x1u, pf_limit.address_type());

  config::PrefetchableMemoryBaseUpper.FromValue(++value).WriteTo(cfg);
  EXPECT_EQ(value, bridge->prfetchable_memory_base_upper());
  EXPECT_EQ(value, config::PrefetchableMemoryBaseUpper.ReadFrom(cfg).reg_value());

  config::PrefetchableMemoryLimitUpper.FromValue(++value).WriteTo(cfg);
  EXPECT_EQ(value, bridge->prfetchable_memory_limit_upper());
  EXPECT_EQ(value, config::PrefetchableMemoryLimitUpper.ReadFrom(cfg).reg_value());

  config::IoBaseUpper.FromValue(static_cast<uint16_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, bridge->io_base_upper());
  EXPECT_EQ(value, config::IoBaseUpper.ReadFrom(cfg).reg_value());

  config::IoLimitUpper.FromValue(static_cast<uint16_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, bridge->io_limit_upper());
  EXPECT_EQ(value, config::IoLimitUpper.ReadFrom(cfg).reg_value());

  config::BridgeExpansionRomAddress.FromValue(++value).WriteTo(cfg);
  EXPECT_EQ(value, bridge->expansion_rom_address());
  EXPECT_EQ(value, config::BridgeExpansionRomAddress.ReadFrom(cfg).reg_value());

  config::BridgeControl.FromValue(static_cast<uint16_t>(++value)).WriteTo(cfg);
  EXPECT_EQ(value, bridge->bridge_control());
  EXPECT_EQ(value, config::BridgeControl.ReadFrom(cfg).reg_value());
}

void MmioConfigGetViewImpl(FakeEcam& ecam) {
  zx::result<fdf::MmioView> view = ecam.CreateMmioConfig(kDefaultBdf1)->get_view();
  ASSERT_OK(view);
  ASSERT_EQ(view->get_size(), ecam.config_size());
  ASSERT_EQ(view->get_offset(), ecam.GetConfigOffset(kDefaultBdf1));
  ASSERT_EQ(view->get_vmo()->get(), ecam.mmio().get_vmo()->get());
}
TEST_F(PciConfigBaseTests, DeviceConfigIntegration) { DeviceConfigIntegrationImpl(ecam()); }

TEST_F(PciConfigExtendedTests, DeviceConfigIntegration) { DeviceConfigIntegrationImpl(ecam()); }

TEST_F(PciConfigBaseTests, MmioConfigReadWrite) {
  auto cfg = ecam().CreateMmioConfig(kDefaultBdf1);
  auto* dev = ecam().get_device(kDefaultBdf1);
  ConfirmDevConfigReadResetImpl(cfg.get(), dev);
  ConfigReadWriteIntegrationImpl(cfg.get(), dev);

  auto bridge_cfg = ecam().CreateMmioConfig(kDefaultBdf2);
  auto* bridge = ecam().get_bridge(kDefaultBdf2);
  BridgeConfigReadWriteIntegrationImpl(bridge_cfg.get(), bridge);
}

TEST_F(PciConfigExtendedTests, MmioConfigReadWrite) {
  auto cfg = ecam().CreateMmioConfig(kDefaultBdf1);
  auto* dev = ecam().get_device(kDefaultBdf1);
  ConfirmDevConfigReadResetImpl(cfg.get(), dev);
  ConfigReadWriteIntegrationImpl(cfg.get(), dev);

  auto bridge_cfg = ecam().CreateMmioConfig(kDefaultBdf2);
  auto* bridge = ecam().get_bridge(kDefaultBdf2);
  BridgeConfigReadWriteIntegrationImpl(bridge_cfg.get(), bridge);
}

TEST_F(PciConfigBaseTests, ProxyConfigReadWrite) {
  auto cfg = ProxyConfig::Create(kDefaultBdf1, config_client());
  auto* dev = ecam().get_device(kDefaultBdf1);
  ConfigReadWriteIntegrationImpl(cfg.value().get(), dev);

  auto bridge_cfg = ProxyConfig::Create(kDefaultBdf2, config_client());
  auto* bridge = ecam().get_bridge(kDefaultBdf2);
  BridgeConfigReadWriteIntegrationImpl(bridge_cfg.value().get(), bridge);
}

TEST_F(PciConfigExtendedTests, ProxyConfigReadWrite) {
  auto cfg = ProxyConfig::Create(kDefaultBdf1, config_client());
  auto* dev = ecam().get_device(kDefaultBdf1);
  ConfigReadWriteIntegrationImpl(cfg.value().get(), dev);

  auto bridge_cfg = ProxyConfig::Create(kDefaultBdf2, config_client());
  auto* bridge = ecam().get_bridge(kDefaultBdf2);
  BridgeConfigReadWriteIntegrationImpl(bridge_cfg.value().get(), bridge);
}

TEST_F(PciConfigBaseTests, ProxyConfigGetView) {
  auto proxy_cfg = ProxyConfig::Create(kDefaultBdf1, config_client());
  ASSERT_EQ(ZX_ERR_NOT_SUPPORTED, proxy_cfg->get_view().status_value());
}

TEST_F(PciConfigBaseTests, MmioConfigGetView) { MmioConfigGetViewImpl(ecam()); }
TEST_F(PciConfigExtendedTests, MmioConfigGetView) { MmioConfigGetViewImpl(ecam()); }

TEST_F(PciConfigBaseTests, FindCapability) {
  auto cfg = ecam().CreateMmioConfig(kDefaultBdf1);
  auto view = ecam().get_config_view(kDefaultBdf1);

  // If Status.has_capabilities_list is false, FindCapability must return std::nullopt.
  config::CapabilitiesPtr.FromValue(0x80).WriteTo(cfg.get());
  EXPECT_EQ(FindCapability(*cfg, Capability::Id::kPciExpress), std::nullopt);

  config::Status.FromValue(0).set_has_capabilities_list(1).WriteTo(cfg.get());

  config::CapabilitiesPtr.FromValue(0).WriteTo(cfg.get());
  EXPECT_EQ(FindCapability(*cfg, Capability::Id::kPciExpress), std::nullopt);

  // Reserved low bits in CapabilitiesPtr and capability next pointers must be masked off.
  config::CapabilitiesPtr
      .FromValue(static_cast<uint8_t>(
          0x80u | static_cast<uint8_t>(~static_cast<uint32_t>(kCapabilityPtrMask))))
      .WriteTo(cfg.get());
  uint8_t cap_80[2] = {
      std::to_underlying(Capability::Id::kPciExpress),
      static_cast<uint8_t>(0x90u |
                           static_cast<uint8_t>(~static_cast<uint32_t>(kCapabilityPtrMask))),
  };
  view.WriteBuffer(0x80, cap_80, sizeof(cap_80));
  uint8_t cap_90[2] = {std::to_underlying(Capability::Id::kMsiX), 0x00};
  view.WriteBuffer(0x90, cap_90, sizeof(cap_90));

  EXPECT_EQ(FindCapability(*cfg, Capability::Id::kPciExpress), 0x80);
  EXPECT_EQ(FindCapability(*cfg, Capability::Id::kMsiX), 0x90);
  EXPECT_EQ(FindCapability(*cfg, Capability::Id::kPciPowerManagement), std::nullopt);

  // A capability pointer into the config header (below kCapPtrMinValid) must be rejected
  // even if those bytes match the target capability ID.
  constexpr uint8_t kBelowHeaderOffset = kCapPtrMinValid - (2 * kCapabilityAlignment);
  view.WriteBuffer(kBelowHeaderOffset, cap_90, sizeof(cap_90));
  config::CapabilitiesPtr.FromValue(kBelowHeaderOffset).WriteTo(cfg.get());
  EXPECT_EQ(FindCapability(*cfg, Capability::Id::kMsiX), std::nullopt);
  config::CapabilitiesPtr.FromValue(0x80).WriteTo(cfg.get());
  cap_80[1] = kBelowHeaderOffset;
  view.WriteBuffer(0x80, cap_80, sizeof(cap_80));
  EXPECT_EQ(FindCapability(*cfg, Capability::Id::kMsiX), std::nullopt);

  // Offset kCapPtrMaxValid is rejected by the last-DWORD guard.
  EXPECT_FALSE(ReadCapability(*cfg, kCapPtrMaxValid).has_value());
  config::CapabilitiesPtr.FromValue(kCapPtrMaxValid).WriteTo(cfg.get());
  EXPECT_EQ(FindCapability(*cfg, Capability::Id::kPciExpress), std::nullopt);
  EXPECT_EQ(ReadExtCapability(*cfg, kPcieExtCapBasePtr), std::nullopt);

  // A cyclic capability chain must terminate via the loop guard.
  config::CapabilitiesPtr.FromValue(0x80).WriteTo(cfg.get());
  cap_80[1] = 0x90;
  view.WriteBuffer(0x80, cap_80, sizeof(cap_80));
  cap_90[1] = 0x80;
  view.WriteBuffer(0x90, cap_90, sizeof(cap_90));
  EXPECT_EQ(FindCapability(*cfg, Capability::Id::kPciPowerManagement), std::nullopt);
}

}  // namespace
}  // namespace pci
