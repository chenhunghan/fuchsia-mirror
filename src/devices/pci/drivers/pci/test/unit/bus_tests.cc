// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.pci.root/cpp/fidl.h>
#include <fidl/fuchsia.hardware.pci/cpp/fidl.h>
#include <lib/driver/legacy-bind-constants/legacy-bind-constants.h>
#include <lib/driver/metadata/cpp/metadata_server.h>
#include <lib/driver/mmio/cpp/mmio.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/fake-resource/resource.h>
#include <lib/fit/defer.h>
#include <lib/pci/hw.h>
#include <lib/zx/bti.h>
#include <lib/zx/clock.h>
#include <lib/zx/result.h>
#include <lib/zx/time.h>
#include <threads.h>
#include <zircon/errors.h>
#include <zircon/limits.h>
#include <zircon/syscalls/object.h>
#include <zircon/syscalls/port.h>

#include <algorithm>
#include <cstddef>
#include <memory>
#include <optional>
#include <vector>

#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/pci/cpp/bind.h>
#include <fbl/auto_lock.h>
#include <fbl/ref_ptr.h>
#include <gtest/gtest.h>

#include "src/devices/pci/drivers/pci/bridge.h"
#include "src/devices/pci/drivers/pci/bus.h"
#include "src/devices/pci/drivers/pci/test/fakes/fake_root_service.h"
#include "src/lib/testing/predicates/status.h"

namespace pci {
namespace {

class FakeCompositeNodeManager
    : public fidl::Server<fuchsia_driver_framework::CompositeNodeManager> {
 public:
  void AddSpec(AddSpecRequest& request, AddSpecCompleter::Sync& completer) override {
    if (epitaph_.has_value()) {
      completer.Close(*epitaph_);
      return;
    }
    if (status_.has_value()) {
      completer.Reply(fit::error(*status_));
      return;
    }
    specs_.push_back(std::move(request));
    completer.Reply(fit::ok());
  }
  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_driver_framework::CompositeNodeManager> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {}

  const std::vector<fuchsia_driver_framework::CompositeNodeSpec>& specs() const { return specs_; }
  void set_status(std::optional<fuchsia_driver_framework::CompositeNodeSpecError> status) {
    status_ = status;
  }
  void set_epitaph(std::optional<zx_status_t> epitaph) { epitaph_ = epitaph; }

 private:
  std::vector<fuchsia_driver_framework::CompositeNodeSpec> specs_;
  std::optional<fuchsia_driver_framework::CompositeNodeSpecError> status_;
  std::optional<zx_status_t> epitaph_;
};

class PciBusTestEnvironment : public fdf_testing::Environment {
 public:
  PciBusTestEnvironment() {
    root_server_.set_platform_info_fn([this] { return MakePlatformInfo(); });
  }

  FakeEcam& ecam() { return *ecam_; }
  std::shared_ptr<FakeEcam> shared_ecam() { return ecam_; }
  FakeRootServer& root_server() { return root_server_; }
  FakeConfigServer& config_server() { return config_server_; }

  void SetExtended(bool is_extended) {
    ecam_ = std::make_shared<FakeEcam>(0, 2, is_extended);
    root_server_.set_ecam(ecam_.get());
    config_server_.set_ecam(*ecam_);
  }

  void SetMetadata(fuchsia_hardware_pci::BoardConfiguration metadata) {
    metadata_ = std::move(metadata);
  }

  void set_serve_root_service(bool serve) { serve_root_service_ = serve; }
  FakeCompositeNodeManager& composite_manager() { return composite_manager_; }

  std::vector<fuchsia_hardware_pci_root::IrqRoutingEntry>& routing_entries() {
    return routing_entries_;
  }
  std::vector<fuchsia_hardware_pci::Bdf>& acpi_devices() { return acpi_devices_; }
  std::vector<fuchsia_hardware_pci::Bdf>& devicetree_devices() { return devicetree_devices_; }

  fuchsia_hardware_pci_root::PlatformInfo MakePlatformInfo() const {
    std::unique_ptr<fuchsia_hardware_pci_root::ParentBdfs> parent_bdfs;
    if (!acpi_devices_.empty()) {
      parent_bdfs = std::make_unique<fuchsia_hardware_pci_root::ParentBdfs>(
          fuchsia_hardware_pci_root::ParentBdfs::WithAcpi(acpi_devices_));
    } else if (!devicetree_devices_.empty()) {
      parent_bdfs = std::make_unique<fuchsia_hardware_pci_root::ParentBdfs>(
          fuchsia_hardware_pci_root::ParentBdfs::WithDevicetree(devicetree_devices_));
    }
    return fuchsia_hardware_pci_root::PlatformInfo{{
        .name = "fakeroot",
        .start_bus_num = 0,
        .end_bus_num = 2,
        .segment_group = 0,
        .irq_routing = routing_entries_,
        .parent_bdfs = std::move(parent_bdfs),
    }};
  }

  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();

    if (serve_root_service_) {
      fuchsia_hardware_pci_root::RootService::InstanceHandler root_handler({
          .root =
              root_bindings_.CreateHandler(&root_server_, dispatcher, fidl::kIgnoreBindingClosure),
      });
      if (zx::result result = to_driver_vfs.AddService<fuchsia_hardware_pci_root::RootService>(
              std::move(root_handler));
          result.is_error()) {
        return result.take_error();
      }
    }

    fuchsia_hardware_pci_root::ConfigService::InstanceHandler config_handler({
        .config = config_bindings_.CreateHandler(
            &config_server_, fdf::Dispatcher::GetCurrent()->get(), fidl::kIgnoreBindingClosure),
    });
    if (zx::result result = to_driver_vfs.AddService<fuchsia_hardware_pci_root::ConfigService>(
            std::move(config_handler));
        result.is_error()) {
      return result.take_error();
    }

    if (metadata_.has_value()) {
      if (zx::result result = metadata_server_.Serve(to_driver_vfs, dispatcher, *metadata_);
          result.is_error()) {
        return result.take_error();
      }
    }

    if (zx::result result =
            to_driver_vfs.component()
                .AddUnmanagedProtocol<fuchsia_driver_framework::CompositeNodeManager>(
                    composite_manager_bindings_.CreateHandler(&composite_manager_, dispatcher,
                                                              fidl::kIgnoreBindingClosure));
        result.is_error()) {
      return result.take_error();
    }

    return zx::ok();
  }

 private:
  std::shared_ptr<FakeEcam> ecam_ = std::make_shared<FakeEcam>(0, 2, /*is_extended=*/true);
  FakeRootServer root_server_{ecam_.get()};
  FakeConfigServer config_server_{*ecam_};
  FakeCompositeNodeManager composite_manager_;
  fidl::ServerBindingGroup<fuchsia_hardware_pci_root::Root> root_bindings_;
  fdf::ServerBindingGroup<fuchsia_hardware_pci_root::Config> config_bindings_;
  fidl::ServerBindingGroup<fuchsia_driver_framework::CompositeNodeManager>
      composite_manager_bindings_;
  fdf_metadata::MetadataServer<fuchsia_hardware_pci::BoardConfiguration> metadata_server_;
  std::optional<fuchsia_hardware_pci::BoardConfiguration> metadata_;
  std::vector<fuchsia_hardware_pci_root::IrqRoutingEntry> routing_entries_;
  std::vector<fuchsia_hardware_pci::Bdf> acpi_devices_;
  std::vector<fuchsia_hardware_pci::Bdf> devicetree_devices_;
  bool serve_root_service_ = true;
};

class TestBus : public pci::Bus {
 public:
  TestBus() = default;

  static DriverRegistration GetDriverRegistration() {
    return FUCHSIA_DRIVER_REGISTRATION_V1(fdf_internal::DriverServer2<TestBus>::initialize,
                                          fdf_internal::DriverServer2<TestBus>::destroy);
  }

  using pci::Bus::board_config;
  using pci::Bus::devices;
  using pci::Bus::devices_lock;
  using pci::Bus::HandleDeviceLegacyIrqUntriggered;
  using pci::Bus::HandleLegacyIrq;
  using pci::Bus::shared_irqs;

  pci::DeviceTree& Devices() { return devices(); }
  const fuchsia_hardware_pci::BoardConfiguration& GetBoardConfiguration() { return board_config(); }

  size_t GetDeviceCount() {
    fbl::AutoLock _(devices_lock());
    return devices().size();
  }

  pci::Device* GetDevice(fuchsia_hardware_pci::wire::Bdf bdf) {
    fbl::AutoLock _(devices_lock());
    auto it = devices().find(bdf);
    return it != devices().end() ? it->second.get() : nullptr;
  }

  size_t GetSharedIrqCount() {
    fbl::AutoLock _(devices_lock());
    return shared_irqs().size();
  }
};

struct FixtureConfig final {
  using DriverType = TestBus;
  using EnvironmentType = PciBusTestEnvironment;
};

class PciBusTests : public ::testing::Test {
 protected:
  void TearDown() override {
    if (driver_started_) {
      if (!driver_stopped_) {
        EXPECT_OK(driver_test_.StopDriver());
      }
      driver_test_.ShutdownAndDestroyDriver();
      driver_started_ = false;
      driver_stopped_ = false;
    }
  }

  zx::result<> StartDriver() {
    zx::result result = driver_test_.StartDriver();
    if (result.is_ok()) {
      driver_started_ = true;
      driver_stopped_ = false;
    }
    return result;
  }

  zx::result<> StopDriver() {
    zx::result result = driver_test_.StopDriver();
    if (result.is_ok()) {
      driver_stopped_ = true;
    }
    return result;
  }

  TestBus* bus() { return driver_test_.driver(); }
  fdf_testing::ForegroundDriverTest<FixtureConfig>& driver_test() { return driver_test_; }

  FakeEcam& ecam() {
    if (!ecam_) {
      ecam_ = driver_test_.RunInEnvironmentTypeContext<std::shared_ptr<FakeEcam>>(
          [](PciBusTestEnvironment& env) { return env.shared_ecam(); });
    }
    return *ecam_;
  }

  zx_info_bti_t GetRootBtiInfo() {
    return driver_test_.RunInEnvironmentTypeContext<zx_info_bti_t>([](PciBusTestEnvironment& env) {
      zx_info_bti_t info{};
      ZX_ASSERT(env.root_server().bti().get_info(ZX_INFO_BTI, &info, sizeof(info), nullptr,
                                                 nullptr) == ZX_OK);
      return info;
    });
  }

  static bool IsLegacyIrqDisabled(pci::Device* dev) {
    fbl::AutoLock _(dev->dev_lock());
    return dev->irqs().legacy_disabled;
  }

  static bool IsCommandIntDisabled(FakeEcam& ecam, fuchsia_hardware_pci::wire::Bdf bdf) {
    return ecam.get_device(bdf)->interrupt_disable();
  }
  bool IsCommandIntDisabled(fuchsia_hardware_pci::wire::Bdf bdf) {
    return IsCommandIntDisabled(ecam(), bdf);
  }

  void SetLegacyIrq(uint32_t vector, zx::interrupt interrupt) {
    driver_test_.RunInEnvironmentTypeContext(
        [vector, interrupt = std::move(interrupt)](PciBusTestEnvironment& env) mutable {
          env.root_server().legacy_irqs()[vector] = std::move(interrupt);
        });
  }

  void AddDevicetreeDevice(fuchsia_hardware_pci::Bdf bdf) {
    driver_test_.RunInEnvironmentTypeContext(
        [bdf = std::move(bdf)](PciBusTestEnvironment& env) mutable {
          env.devicetree_devices().push_back(std::move(bdf));
        });
  }

  void AddAcpiDevice(fuchsia_hardware_pci::Bdf bdf) {
    driver_test_.RunInEnvironmentTypeContext(
        [bdf = std::move(bdf)](PciBusTestEnvironment& env) mutable {
          env.acpi_devices().push_back(std::move(bdf));
        });
  }

  std::vector<fuchsia_driver_framework::CompositeNodeSpec> GetCompositeSpecs() {
    return driver_test_
        .RunInEnvironmentTypeContext<std::vector<fuchsia_driver_framework::CompositeNodeSpec>>(
            [](PciBusTestEnvironment& env) { return env.composite_manager().specs(); });
  }

  uint32_t SetupTopology() {
    return driver_test_.RunInEnvironmentTypeContext<uint32_t>([](PciBusTestEnvironment& env) {
      uint8_t idx = 1;
      auto& ecam = env.ecam();
      ecam.get_device({.bus_id = 0, .device_id = 0, .function_id = 0})
          ->set_vendor_id(0x8086)
          .set_device_id(idx++)
          .set_header_type(config::HeaderType.FromValue(0).set_multi_function(1).reg_value());
      ecam.get_device({.bus_id = 0, .device_id = 0, .function_id = 1})
          ->set_vendor_id(0x8086)
          .set_device_id(idx++);
      ecam.get_bridge({.bus_id = 0, .device_id = 1, .function_id = 0})
          ->set_vendor_id(0x8086)
          .set_device_id(idx++)
          .set_header_type(kHeaderTypePciBridge)
          .set_io_base(0x10)
          .set_io_limit(0x0FFF)
          .set_memory_base(0x1000)
          .set_memory_limit(0xFFFFFFFF)
          .set_secondary_bus_number(1);
      ecam.get_device({.bus_id = 1, .device_id = 0, .function_id = 0})
          ->set_vendor_id(0x8086)
          .set_device_id(idx++)
          .set_header_type(config::HeaderType.FromValue(0).set_multi_function(1).reg_value());
      ecam.get_device({.bus_id = 1, .device_id = 0, .function_id = 1})
          ->set_vendor_id(0x8086)
          .set_device_id(idx);
      return idx;
    });
  }

  uint32_t SetupMultifunctionTopology() {
    return driver_test_.RunInEnvironmentTypeContext<uint32_t>([](PciBusTestEnvironment& env) {
      uint8_t idx = 1;
      auto& ecam = env.ecam();
      ecam.get_device({.bus_id = 0, .device_id = 0, .function_id = 0})
          ->set_vendor_id(0x8086)
          .set_device_id(idx++)
          .set_header_type(config::HeaderType.FromValue(0).set_multi_function(1).reg_value());
      ecam.get_bridge({.bus_id = 0, .device_id = 0, .function_id = 1})
          ->set_vendor_id(0x8086)
          .set_device_id(idx++)
          .set_header_type(config::HeaderType.FromValue(0)
                               .set_type(kHeaderTypePciBridge)
                               .set_multi_function(1)
                               .reg_value())
          .set_io_base(0x10)
          .set_io_limit(0x0FFF)
          .set_memory_base(0x1000)
          .set_memory_limit(0xFFFFFFFF)
          .set_secondary_bus_number(1);
      ecam.get_device({.bus_id = 0, .device_id = 0, .function_id = 2})
          ->set_vendor_id(0x8086)
          .set_device_id(idx++);
      ecam.get_device({.bus_id = 1, .device_id = 0, .function_id = 0})
          ->set_vendor_id(0x8086)
          .set_device_id(idx++);
      ecam.get_device({.bus_id = 1, .device_id = 1, .function_id = 0})
          ->set_vendor_id(0x8086)
          .set_device_id(idx);
      return idx;
    });
  }

  zx::interrupt AddLegacyIrqToBus(uint8_t vector) {
    zx::interrupt interrupt;
    ZX_ASSERT(zx::interrupt::create(*zx::unowned_resource(ZX_HANDLE_INVALID), vector,
                                    ZX_INTERRUPT_VIRTUAL, &interrupt) == ZX_OK);
    zx::interrupt bus_interrupt;
    ZX_ASSERT(interrupt.duplicate(ZX_RIGHT_SAME_RIGHTS, &bus_interrupt) == ZX_OK);
    driver_test_.RunInEnvironmentTypeContext(
        [vector, bus_interrupt = std::move(bus_interrupt)](PciBusTestEnvironment& env) mutable {
          env.root_server().legacy_irqs()[vector] = std::move(bus_interrupt);
        });
    return interrupt;
  }

  void AddRoutingEntryToBus(std::optional<uint8_t> p_dev, std::optional<uint8_t> p_func,
                            uint8_t dev_id, uint8_t a, uint8_t b, uint8_t c, uint8_t d) {
    driver_test_.RunInEnvironmentTypeContext([=](PciBusTestEnvironment& env) {
      env.routing_entries().push_back(fuchsia_hardware_pci_root::IrqRoutingEntry{{
          .port_device_id =
              p_dev ? *p_dev : static_cast<uint8_t>(fuchsia_hardware_pci_root::kIrqRoutingNoParent),
          .port_function_id =
              p_func ? *p_func
                     : static_cast<uint8_t>(fuchsia_hardware_pci_root::kIrqRoutingNoParent),
          .device_id = dev_id,
          .pins = {{a, b, c, d}},
      }});
    });
  }

  void StopAndDestroyDriver() {
    if (!driver_stopped_) {
      ASSERT_OK(driver_test_.StopDriver());
      driver_stopped_ = true;
    }
    driver_test_.ShutdownAndDestroyDriver();
    driver_started_ = false;
    driver_stopped_ = false;
  }

 private:
  fdf_testing::ForegroundDriverTest<FixtureConfig> driver_test_;
  std::shared_ptr<FakeEcam> ecam_;
  bool driver_started_ = false;
  bool driver_stopped_ = false;
};

// Tests that the bus driver starts successfully, enumerates downstream devices,
// and creates child nodes.
TEST_F(PciBusTests, StartBus) {
  SetupTopology();
  ASSERT_OK(StartDriver());
  EXPECT_EQ(bus()->GetDeviceCount(), 5u);
  driver_test().RunInNodeContext([](fdf_testing::TestNode& node) {
    ASSERT_EQ(node.children().count("bus"), 1u);
    EXPECT_EQ(node.children().at("bus").children().size(), 5u);
  });
}

// Tests that downstream device child nodes are published with the expected NodeProperty2
// properties.
TEST_F(PciBusTests, ChildNodeProperties) {
  SetupTopology();
  ASSERT_OK(StartDriver());

  driver_test().RunInNodeContext([](fdf_testing::TestNode& node) {
    ASSERT_EQ(node.children().count("bus"), 1u);
    auto& bus_node = node.children().at("bus");
    ASSERT_EQ(bus_node.children().count("00_00_0"), 1u);
    const auto& child = bus_node.children().at("00_00_0");
    auto properties = child.GetProperties();

    auto assert_int_property = [&](std::string_view key, uint32_t expected_value) {
      auto prop = std::ranges::find_if(properties, [&](const auto& p) { return p.key() == key; });
      ASSERT_NE(prop, properties.end());
      ASSERT_EQ(prop->value().Which(), fuchsia_driver_framework::NodePropertyValue::Tag::kIntValue);
      EXPECT_EQ(prop->value().int_value().value(), expected_value);
    };

    assert_int_property(bind_fuchsia::PCI_VID, 0x8086u);
    assert_int_property(bind_fuchsia::PCI_DID, 1u);
    assert_int_property(bind_fuchsia::PCI_CLASS, 0u);
    assert_int_property(bind_fuchsia::PCI_TOPO, static_cast<uint32_t>(BIND_PCI_TOPO_PACK(0, 0, 0)));
    assert_int_property(bind_fuchsia_pci::SEGMENT, 0u);
  });
}

// Tests that child nodes are removed from the topology when the bus driver stops.
TEST_F(PciBusTests, StopRemovesChildNodes) {
  SetupTopology();
  ASSERT_OK(StartDriver());
  EXPECT_EQ(bus()->GetDeviceCount(), 5u);

  size_t child_count = driver_test().RunInNodeContext<size_t>(
      [](fdf_testing::TestNode& node) { return node.children().at("bus").children().size(); });
  EXPECT_EQ(child_count, 5u);

  ASSERT_OK(StopDriver());

  driver_test().runtime().RunUntil([&]() {
    return driver_test().RunInNodeContext<size_t>(
               [](fdf_testing::TestNode& node) { return node.children().size(); }) == 0u;
  });
}

// Tests connecting to exposed device and debug services through the driver's outgoing directory.
TEST_F(PciBusTests, OutgoingServices) {
  SetupTopology();
  ASSERT_OK(StartDriver());

  zx::result device_client_end =
      driver_test().Connect<fuchsia_hardware_pci::Service::Device>("00_00_0");
  ASSERT_OK(device_client_end);

  zx::result debug_client_end = driver_test().Connect<fuchsia_hardware_pci::DebugService::Debug>();
  ASSERT_OK(debug_client_end);

  zx::result run_result = driver_test().RunOnBackgroundDispatcherSync([&]() {
    fidl::SyncClient device_client(std::move(*device_client_end));
    auto device_info = device_client->GetDeviceInfo();
    ASSERT_TRUE(device_info.is_ok());
    EXPECT_EQ(device_info->info().vendor_id(), static_cast<uint16_t>(0x8086));
    EXPECT_EQ(device_info->info().device_id(), static_cast<uint16_t>(1));

    fidl::SyncClient debug_client(std::move(*debug_client_end));
    auto bridge_info = debug_client->GetHostBridgeInfo();
    ASSERT_TRUE(bridge_info.is_ok());
    EXPECT_EQ(bridge_info->info().name(), "fakeroot");
    EXPECT_EQ(bridge_info->info().start_bus_number(), static_cast<uint8_t>(0));
    EXPECT_EQ(bridge_info->info().end_bus_number(), static_cast<uint8_t>(2));
  });
  ASSERT_OK(run_result);
}

TEST_F(PciBusTests, BindProxyConfig) {
  driver_test().RunInEnvironmentTypeContext([](PciBusTestEnvironment& env) {
    env.SetExtended(false);
    env.root_server().set_enable_cam(false);
  });
  uint32_t dev_cnt = SetupTopology();
  ASSERT_OK(StartDriver());
  EXPECT_EQ(bus()->GetDeviceCount(), dev_cnt);
}

TEST_F(PciBusTests, StartFailsWhenNoRootService) {
  driver_test().RunInEnvironmentTypeContext(
      [](PciBusTestEnvironment& env) { env.set_serve_root_service(false); });
  EXPECT_TRUE(StartDriver().is_error());
}

TEST_F(PciBusTests, StartFailsWhenGetCamFails) {
  driver_test().RunInEnvironmentTypeContext(
      [](PciBusTestEnvironment& env) { env.root_server().set_cam_status(ZX_ERR_INTERNAL); });
  EXPECT_TRUE(StartDriver().is_error());
}

TEST_F(PciBusTests, BdiGetBti) {
  ecam()
      .get_device({.bus_id = 0, .device_id = 0, .function_id = 0})
      ->set_vendor_id(8086)
      .set_device_id(8086);
  ASSERT_OK(StartDriver());
  auto* bus = this->bus();
  ASSERT_EQ(bus->GetDeviceCount(), 1u);

  zx::bti bti = {};
  ASSERT_EQ(bus->GetBti(nullptr, 0, &bti), ZX_ERR_INVALID_ARGS);
  ASSERT_OK(bus->GetBti(bus->GetDevice({0, 0, 0}), 0, &bti));

  zx_info_bti_t info = {};
  zx_info_bti_t info2 = GetRootBtiInfo();
  ASSERT_OK(bti.get_info(ZX_INFO_BTI, &info, sizeof(info), nullptr, nullptr));
  ASSERT_EQ(info.aspace_size, info2.aspace_size);
  ASSERT_EQ(info.minimum_contiguity, info2.minimum_contiguity);
  ASSERT_EQ(info.pmo_count, info2.pmo_count);
  ASSERT_EQ(info.quarantine_count, info2.quarantine_count);
}

TEST_F(PciBusTests, BdiAllocateMsi) {
  ASSERT_OK(StartDriver());
  auto* bus = this->bus();

  for (uint32_t cnt = 1; cnt <= 32; cnt *= 2) {
    zx::msi msi = {};
    fuchsia_hardware_pci_root::wire::MsiAllocationInfo info = {};
    bus->AllocateMsi(cnt, &msi, &info);
    EXPECT_EQ(info.irq_count, cnt);
    EXPECT_EQ(info.target_addr, 0xCAFEu);
    EXPECT_EQ(info.target_data, 0xC0FEu);
  }
}

TEST_F(PciBusTests, BdiGetMsiHandle) {
  ASSERT_OK(StartDriver());
  auto* bus = this->bus();

  zx::msi msi = {};
  fuchsia_hardware_pci_root::wire::MsiAllocationInfo info = {};
  ASSERT_OK(bus->AllocateMsi(2, &msi, &info));

  zx::vmo cfg_vmo;
  ASSERT_OK(zx::vmo::create(4096, 0, &cfg_vmo));
  ASSERT_OK(cfg_vmo.set_cache_policy(ZX_CACHE_POLICY_UNCACHED_DEVICE));

  zx::interrupt interrupt;
  ASSERT_OK(bus->GetMsiHandle(msi, /*options=*/0, 0, cfg_vmo, 0, &interrupt));
  EXPECT_TRUE(interrupt.is_valid());
  interrupt.reset();
}

TEST_F(PciBusTests, BdiLinkUnlinkDevice) {
  ecam()
      .get_device({.bus_id = 0, .device_id = 0, .function_id = 0})
      ->set_vendor_id(8086)
      .set_device_id(8086);
  ASSERT_OK(StartDriver());
  auto* bus = this->bus();
  ASSERT_EQ(bus->GetDeviceCount(), 1u);

  auto device = bus->GetDevice({.bus_id = 0, .device_id = 0, .function_id = 0});
  auto reffed_device = fbl::RefPtr(bus->GetDevice({.bus_id = 0, .device_id = 0, .function_id = 0}));
  EXPECT_EQ(bus->LinkDevice(reffed_device), ZX_ERR_ALREADY_EXISTS);
  EXPECT_OK(bus->UnlinkDevice(device));
  EXPECT_EQ(bus->GetDeviceCount(), 0u);
  EXPECT_EQ(bus->UnlinkDevice(device), ZX_ERR_NOT_FOUND);

  // Insert the device back into the bus topology so the disable / unplug
  // lifecycle runs. Otherwise, the normal teardown path of Device will assert
  // that it was never disabled.
  ASSERT_OK(bus->LinkDevice(fbl::RefPtr(device)));
  ASSERT_EQ(bus->GetDeviceCount(), 1u);
}

TEST_F(PciBusTests, IrqRoutingEntries) {
  // Add |int_cnt| interrupts, but make them share vectors based on |int_mod|. This ensures that
  // we handle duplicate IRQ entries properly.
  const size_t int_cnt = 5;
  const uint32_t int_mod = 3;
  zx::interrupt interrupt = {};
  for (uint32_t i = 0; i < int_cnt; i++) {
    ASSERT_OK(zx::interrupt::create(*zx::unowned_resource(ZX_HANDLE_INVALID), i,
                                    ZX_INTERRUPT_VIRTUAL, &interrupt));
    SetLegacyIrq((i % int_mod) + 1, std::move(interrupt));
  }
  ecam()
      .get_device({.bus_id = 0, .device_id = 0, .function_id = 0})
      ->set_vendor_id(1)
      .set_device_id(2)
      .set_interrupt_pin(1);

  AddRoutingEntryToBus(/*p_dev=*/std::nullopt, /*p_func=*/std::nullopt, /*dev_id=*/0,
                       /*a=*/1, /*b=*/2, /*c=*/3, /*d=*/4);

  ASSERT_OK(StartDriver());
  auto* bus = this->bus();
  ASSERT_EQ(int_mod, bus->GetSharedIrqCount());
}

TEST_F(PciBusTests, IrqRoutingMissingVectorSkipped) {
  ecam()
      .get_device({.bus_id = 0, .device_id = 0, .function_id = 0})
      ->set_vendor_id(1)
      .set_device_id(2)
      .set_interrupt_pin(1);
  AddRoutingEntryToBus(/*p_dev=*/std::nullopt, /*p_func=*/std::nullopt, /*dev_id=*/0,
                       /*a=*/42, /*b=*/0, /*c=*/0, /*d=*/0);

  ASSERT_OK(StartDriver());
  EXPECT_EQ(0u, bus()->GetSharedIrqCount());
}

TEST_F(PciBusTests, IrqRoutingInjectedErrorSkipped) {
  driver_test().RunInEnvironmentTypeContext([](PciBusTestEnvironment& env) {
    env.root_server().set_get_legacy_interrupt_status(ZX_ERR_INTERNAL);
  });
  ecam()
      .get_device({.bus_id = 0, .device_id = 0, .function_id = 0})
      ->set_vendor_id(1)
      .set_device_id(2)
      .set_interrupt_pin(1);
  AddRoutingEntryToBus(/*p_dev=*/std::nullopt, /*p_func=*/std::nullopt, /*dev_id=*/0,
                       /*a=*/1, /*b=*/2, /*c=*/3, /*d=*/4);

  ASSERT_OK(StartDriver());
  EXPECT_EQ(0u, bus()->GetSharedIrqCount());
}

TEST_F(PciBusTests, LegacyIrqSignalTest) {
  // Establish the IRQ in the Pciroot implementation so that the bus will configure our device to
  // use it if the device id is 0x1 and it uses pin B.
  uint32_t vector = 0xA;
  zx::interrupt interrupt = AddLegacyIrqToBus(vector);
  AddRoutingEntryToBus(/*p_dev=*/std::nullopt, /*p_func=*/std::nullopt, /*dev_id=*/0,
                       /*a=*/vector,
                       /*b=*/vector, /*c=*/0, /*d=*/0);
  // Have the routing table target device 0, pin B. This is configured in
  // SetupTopology for the device itself.
  SetupTopology();
  // These devices need interrupt pins mapped before Bus scans the topology.
  ecam().get_device({.bus_id = 0, .device_id = 0, .function_id = 0})->set_interrupt_pin(0x1);
  ecam().get_device({.bus_id = 0, .device_id = 0, .function_id = 1})->set_interrupt_pin(0x2);
  ASSERT_OK(StartDriver());
  auto* bus = this->bus();
  ASSERT_EQ(1u, bus->GetSharedIrqCount());

  zx::interrupt dev_interrupt[2];
  // Configure both devices and map their driver facing interrupts. They have
  // different pins, but the pins are mapped to the same vector.
  for (uint8_t i = 0; i < 2; i++) {
    auto* bus_device = bus->GetDevice({0, 0, i});
    ASSERT_OK(bus_device->SetIrqMode(fuchsia_hardware_pci::InterruptMode::kLegacy, 1u));
    // Map the interrupt the same way a driver would.
    auto result = bus->GetDevice({0, 0, i})->MapInterrupt(0);
    ASSERT_TRUE(result.is_ok());
    dev_interrupt[i] = std::move(result.value());
  }

  // Bind device 00:00.0's interrupt to a port so we can "peek" at the interrupt
  // status via a port wait.
  zx::port port;
  ASSERT_OK(zx::port::create(ZX_PORT_BIND_TO_INTERRUPT, &port));
  ASSERT_OK(dev_interrupt[0].bind(port, 1, ZX_INTERRUPT_BIND));

  // Here we simulate triggering the hardware vector and track it all the way to
  // the interrupt event a downstream driver bound to this device would get.
  // Timestamps of the original vector must match.
  zx::time_boot receive_time;
  zx::time_boot trigger_time = zx::clock::get_boot();
  ecam().get_device({.bus_id = 0, .device_id = 0, .function_id = 1})->set_status(kStatusInterrupt);
  ASSERT_OK(interrupt.trigger(0, trigger_time));

  // Process the interrupt event on the async dispatcher.
  driver_test().runtime().RunUntilIdle();

  // Only the device at 00:00.1 should trigger because 00:00.0 does not have the interrupt status
  // bit set in its config space. The interrupt time the driver receives must match the time the
  // interrupt dispatcher logged.
  ASSERT_OK(dev_interrupt[1].wait(&receive_time));
  ASSERT_EQ(trigger_time, receive_time);

  // If we handled the interrupt status check then there should be no packet on this port.
  zx_port_packet_t packet{};
  ASSERT_EQ(ZX_ERR_TIMED_OUT, port.wait(zx::deadline_after(zx::sec(0)), &packet));
}

TEST_F(PciBusTests, LegacyIrqMaskOnDeliverTest) {
  // Legacy (INTx) interrupts are level-triggered: a device holds its interrupt
  // line asserted until its driver services it. After the bus driver dispatches
  // the interrupt it must mask the device so the line deasserts; otherwise the
  // kernel re-delivers the still-asserted line in a storm. The mask is cleared
  // again once the driver acknowledges. This test verifies that mask/unmask.
  fuchsia_hardware_pci::wire::Bdf device = {.bus_id = 0, .device_id = 0, .function_id = 0};
  ecam()
      .get_device(device)
      ->set_vendor_id(0x8086)
      .set_device_id(0x8086)
      .set_interrupt_pin(0x1)
      .set_status(kStatusInterrupt);
  // Route pin A to vector 16.
  constexpr uint8_t kVector = 0x10;
  zx::interrupt bus_interrupt = AddLegacyIrqToBus(kVector);
  AddRoutingEntryToBus(/*p_dev=*/std::nullopt, /*p_func=*/std::nullopt, /*dev_id=*/0,
                       /*a=*/kVector,
                       /*b=*/0, /*c=*/0, /*d=*/0);
  ASSERT_OK(StartDriver());
  auto* bus = this->bus();

  auto* bus_device = bus->GetDevice(device);
  ASSERT_OK(bus_device->SetIrqMode(fuchsia_hardware_pci::InterruptMode::kLegacy, 1));
  auto result = bus_device->MapInterrupt(0);
  ASSERT_TRUE(result.is_ok());
  zx::interrupt dev_interrupt = std::move(result.value());

  zx::port port;
  ASSERT_OK(zx::port::create(ZX_PORT_BIND_TO_INTERRUPT, &port));
  ASSERT_OK(dev_interrupt.bind(port, 0, 0));

  ASSERT_FALSE(IsLegacyIrqDisabled(bus_device));
  ASSERT_FALSE(IsCommandIntDisabled(device));

  // Fire the hardware vector. The async IRQ handler signals the device's virtual
  // interrupt and then masks the device.
  ASSERT_OK(bus_interrupt.trigger(0, zx::clock::get_boot()));
  driver_test().runtime().RunUntilIdle();

  zx_port_packet_t packet;
  ASSERT_OK(port.wait(zx::time::infinite(), &packet));
  ASSERT_TRUE(IsLegacyIrqDisabled(bus_device));
  ASSERT_TRUE(IsCommandIntDisabled(device));

  // Acknowledging the virtual interrupt handle re-arms the device automatically
  // via the ZX_VIRTUAL_INTERRUPT_UNTRIGGERED signal handler.
  ASSERT_OK(dev_interrupt.ack());
  driver_test().runtime().RunUntilIdle();
  ASSERT_FALSE(IsLegacyIrqDisabled(bus_device));
  ASSERT_FALSE(IsCommandIntDisabled(device));

  // Trigger a second interrupt to verify consecutive delivery and re-arming.
  ASSERT_OK(bus_interrupt.trigger(0, zx::clock::get_boot()));
  driver_test().runtime().RunUntilIdle();
  ASSERT_OK(port.wait(zx::time::infinite(), &packet));
  ASSERT_TRUE(IsLegacyIrqDisabled(bus_device));
  ASSERT_TRUE(IsCommandIntDisabled(device));

  ASSERT_OK(dev_interrupt.ack());
  driver_test().runtime().RunUntilIdle();
  ASSERT_FALSE(IsLegacyIrqDisabled(bus_device));
  ASSERT_FALSE(IsCommandIntDisabled(device));
}

TEST_F(PciBusTests, LegacyInterruptLifecycleSyncWait) {
  fuchsia_hardware_pci::wire::Bdf device = {.bus_id = 0, .device_id = 0, .function_id = 0};
  ecam()
      .get_device(device)
      ->set_vendor_id(0x8086)
      .set_device_id(0x8086)
      .set_interrupt_pin(0x1)
      .set_status(kStatusInterrupt);
  constexpr uint8_t kVector = 0x10;
  zx::interrupt bus_interrupt = AddLegacyIrqToBus(kVector);
  AddRoutingEntryToBus(/*p_dev=*/std::nullopt, /*p_func=*/std::nullopt, /*dev_id=*/0,
                       /*a=*/kVector, /*b=*/0, /*c=*/0, /*d=*/0);
  ASSERT_OK(StartDriver());
  auto* bus = this->bus();

  auto* bus_device = bus->GetDevice(device);
  ASSERT_OK(bus_device->SetIrqMode(fuchsia_hardware_pci::InterruptMode::kLegacy, 1));
  auto result = bus_device->MapInterrupt(0);
  ASSERT_TRUE(result.is_ok());
  zx::interrupt dev_interrupt = std::move(result.value());

  ASSERT_FALSE(IsLegacyIrqDisabled(bus_device));
  ASSERT_FALSE(IsCommandIntDisabled(device));

  // Trigger physical IRQ vector to signal the virtual interrupt to the waiter thread.
  ASSERT_OK(bus_interrupt.trigger(0, zx::clock::get_boot()));
  driver_test().runtime().RunUntilIdle();

  zx::time_boot receive_time;
  ASSERT_OK(dev_interrupt.wait(&receive_time));
  ASSERT_TRUE(IsLegacyIrqDisabled(bus_device));
  ASSERT_TRUE(IsCommandIntDisabled(device));

  // In synchronous mode, the consumer does not call ack(); instead, its subsequent call
  // to wait() transitions the virtual interrupt to untriggered, unmasking the device.
  thrd_t waiter_thrd;
  auto waiter_entry = [](void* arg) -> int {
    auto* irq = static_cast<zx::interrupt*>(arg);
    zx::time_boot ts;
    return irq->wait(&ts);
  };
  ASSERT_EQ(thrd_create(&waiter_thrd, waiter_entry, &dev_interrupt), thrd_success);
  int thrd_res = 0;
  auto cleanup = fit::defer([&]() {
    dev_interrupt.destroy();
    thrd_join(waiter_thrd, &thrd_res);
  });

  // Wait for the waiter thread to enter wait() and assert ZX_VIRTUAL_INTERRUPT_UNTRIGGERED.
  zx_signals_t observed;
  ASSERT_OK(
      dev_interrupt.wait_one(ZX_VIRTUAL_INTERRUPT_UNTRIGGERED, zx::time::infinite(), &observed));

  driver_test().runtime().RunUntilIdle();
  ASSERT_FALSE(IsLegacyIrqDisabled(bus_device));
  ASSERT_FALSE(IsCommandIntDisabled(device));

  cleanup.call();
  EXPECT_EQ(thrd_res, ZX_ERR_CANCELED);
}

TEST_F(PciBusTests, LegacyInterruptWaitCancellation) {
  fuchsia_hardware_pci::wire::Bdf device = {.bus_id = 0, .device_id = 0, .function_id = 0};
  ecam()
      .get_device(device)
      ->set_vendor_id(0x8086)
      .set_device_id(0x8086)
      .set_interrupt_pin(0x1)
      .set_status(kStatusInterrupt);
  constexpr uint8_t kVector = 0x10;
  zx::interrupt bus_interrupt = AddLegacyIrqToBus(kVector);
  AddRoutingEntryToBus(/*p_dev=*/std::nullopt, /*p_func=*/std::nullopt, /*dev_id=*/0,
                       /*a=*/kVector, /*b=*/0, /*c=*/0, /*d=*/0);
  ASSERT_OK(StartDriver());
  auto* bus = this->bus();
  auto* bus_device = bus->GetDevice(device);
  ASSERT_OK(bus_device->SetIrqMode(fuchsia_hardware_pci::InterruptMode::kLegacy, 1));
  auto result = bus_device->MapInterrupt(0);
  ASSERT_TRUE(result.is_ok());
  zx::interrupt dev_interrupt = std::move(result.value());
  zx::port port;
  ASSERT_OK(zx::port::create(ZX_PORT_BIND_TO_INTERRUPT, &port));
  ASSERT_OK(dev_interrupt.bind(port, 0, 0));

  ASSERT_FALSE(IsLegacyIrqDisabled(bus_device));
  ASSERT_FALSE(IsCommandIntDisabled(device));

  // Fire interrupt to start untriggered wait.
  ASSERT_OK(bus_interrupt.trigger(0, zx::clock::get_boot()));
  driver_test().runtime().RunUntilIdle();
  zx_port_packet_t packet;
  ASSERT_OK(port.wait(zx::time::infinite(), &packet));
  ASSERT_TRUE(IsLegacyIrqDisabled(bus_device));
  ASSERT_TRUE(IsCommandIntDisabled(device));

  // Disabling interrupts cancels the pending wait cleanly.
  ASSERT_OK(bus_device->SetIrqMode(fuchsia_hardware_pci::InterruptMode::kDisabled, 0));
  driver_test().runtime().RunUntilIdle();

  // Acknowledging the handle after disable must not unmask the device.
  ASSERT_OK(dev_interrupt.ack());
  driver_test().runtime().RunUntilIdle();
  {
    fbl::AutoLock _(bus_device->dev_lock());
    EXPECT_EQ(bus_device->irqs().mode, fuchsia_hardware_pci::InterruptMode::kDisabled);
    EXPECT_TRUE(bus_device->irqs().legacy_disabled);
  }
}

TEST_F(PciBusTests, LegacyInterruptSharedVectorConcurrentAssert) {
  fuchsia_hardware_pci::wire::Bdf device_a = {.bus_id = 0, .device_id = 0, .function_id = 0};
  fuchsia_hardware_pci::wire::Bdf device_b = {.bus_id = 0, .device_id = 1, .function_id = 0};
  ecam()
      .get_device(device_a)
      ->set_vendor_id(0x8086)
      .set_device_id(0x8086)
      .set_interrupt_pin(0x1)
      .set_status(kStatusInterrupt);
  ecam().get_device(device_b)->set_vendor_id(0x8086).set_device_id(0x8086).set_interrupt_pin(0x1);
  constexpr uint8_t kVector = 0x10;
  zx::interrupt bus_interrupt = AddLegacyIrqToBus(kVector);
  AddRoutingEntryToBus(/*p_dev=*/std::nullopt, /*p_func=*/std::nullopt, /*dev_id=*/0,
                       /*a=*/kVector, /*b=*/0, /*c=*/0, /*d=*/0);
  AddRoutingEntryToBus(/*p_dev=*/std::nullopt, /*p_func=*/std::nullopt, /*dev_id=*/1,
                       /*a=*/kVector, /*b=*/0, /*c=*/0, /*d=*/0);
  ASSERT_OK(StartDriver());
  auto* bus = this->bus();

  auto* bus_dev_a = bus->GetDevice(device_a);
  auto* bus_dev_b = bus->GetDevice(device_b);
  ASSERT_OK(bus_dev_a->SetIrqMode(fuchsia_hardware_pci::InterruptMode::kLegacy, 1));
  ASSERT_OK(bus_dev_b->SetIrqMode(fuchsia_hardware_pci::InterruptMode::kLegacy, 1));
  auto res_a = bus_dev_a->MapInterrupt(0);
  auto res_b = bus_dev_b->MapInterrupt(0);
  ASSERT_TRUE(res_a.is_ok());
  ASSERT_TRUE(res_b.is_ok());
  zx::interrupt dev_a_irq = std::move(res_a.value());
  zx::interrupt dev_b_irq = std::move(res_b.value());

  zx::port port;
  ASSERT_OK(zx::port::create(ZX_PORT_BIND_TO_INTERRUPT, &port));
  ASSERT_OK(dev_a_irq.bind(port, 1, 0));
  ASSERT_OK(dev_b_irq.bind(port, 2, 0));

  // Assert vector: Device A asserts while Device B is idle.
  ASSERT_OK(bus_interrupt.trigger(0, zx::clock::get_boot()));
  driver_test().runtime().RunUntilIdle();

  zx_port_packet_t packet;
  ASSERT_OK(port.wait(zx::time::infinite(), &packet));
  EXPECT_EQ(packet.key, 1u);
  ASSERT_TRUE(IsLegacyIrqDisabled(bus_dev_a));
  ASSERT_TRUE(IsCommandIntDisabled(device_a));
  ASSERT_FALSE(IsLegacyIrqDisabled(bus_dev_b));
  ASSERT_FALSE(IsCommandIntDisabled(device_b));

  // Vector triggers again while Device A ack is pending; Device B asserts.
  ecam().get_device(device_b)->set_status(kStatusInterrupt);
  ASSERT_OK(bus_interrupt.trigger(0, zx::clock::get_boot()));
  driver_test().runtime().RunUntilIdle();

  // Device B is signaled and masked; Device A was skipped without error.
  ASSERT_OK(port.wait(zx::time::infinite(), &packet));
  EXPECT_EQ(packet.key, 2u);
  ASSERT_TRUE(IsLegacyIrqDisabled(bus_dev_b));
  ASSERT_TRUE(IsCommandIntDisabled(device_b));
  ASSERT_TRUE(IsLegacyIrqDisabled(bus_dev_a));
  ASSERT_TRUE(IsCommandIntDisabled(device_a));

  // Acknowledging Device A unmasks it while Device B remains masked.
  ASSERT_OK(dev_a_irq.ack());
  driver_test().runtime().RunUntilIdle();
  ASSERT_FALSE(IsLegacyIrqDisabled(bus_dev_a));
  ASSERT_FALSE(IsCommandIntDisabled(device_a));
  ASSERT_TRUE(IsLegacyIrqDisabled(bus_dev_b));
  ASSERT_TRUE(IsCommandIntDisabled(device_b));

  // Acknowledging Device B unmasks the remaining device.
  ASSERT_OK(dev_b_irq.ack());
  driver_test().runtime().RunUntilIdle();
  ASSERT_FALSE(IsLegacyIrqDisabled(bus_dev_b));
  ASSERT_FALSE(IsCommandIntDisabled(device_b));
}

TEST_F(PciBusTests, LegacyInterruptWorkaroundPendingSkip) {
  fuchsia_hardware_pci::wire::Bdf device_a = {.bus_id = 0, .device_id = 0, .function_id = 0};
  fuchsia_hardware_pci::wire::Bdf device_b = {.bus_id = 0, .device_id = 1, .function_id = 0};
  ecam().get_device(device_a)->set_vendor_id(0x8086).set_device_id(0x8086).set_interrupt_pin(0x1);
  ecam().get_device(device_b)->set_vendor_id(0x8086).set_device_id(0x8086).set_interrupt_pin(0x1);
  constexpr uint8_t kVector = 0x10;
  zx::interrupt bus_interrupt = AddLegacyIrqToBus(kVector);
  AddRoutingEntryToBus(/*p_dev=*/std::nullopt, /*p_func=*/std::nullopt, /*dev_id=*/0,
                       /*a=*/kVector, /*b=*/0, /*c=*/0, /*d=*/0);
  AddRoutingEntryToBus(/*p_dev=*/std::nullopt, /*p_func=*/std::nullopt, /*dev_id=*/1,
                       /*a=*/kVector, /*b=*/0, /*c=*/0, /*d=*/0);

  // Set use_intx_workaround board config.
  fuchsia_hardware_pci::BoardConfiguration board_config;
  board_config.use_intx_workaround(true);
  driver_test().RunInEnvironmentTypeContext(
      [&](PciBusTestEnvironment& env) { env.SetMetadata(std::move(board_config)); });

  ASSERT_OK(StartDriver());
  auto* bus = this->bus();

  auto* bus_dev_a = bus->GetDevice(device_a);
  auto* bus_dev_b = bus->GetDevice(device_b);
  ASSERT_OK(bus_dev_a->SetIrqMode(fuchsia_hardware_pci::InterruptMode::kLegacy, 1));
  ASSERT_OK(bus_dev_b->SetIrqMode(fuchsia_hardware_pci::InterruptMode::kLegacy, 1));
  auto res_a = bus_dev_a->MapInterrupt(0);
  auto res_b = bus_dev_b->MapInterrupt(0);
  ASSERT_TRUE(res_a.is_ok());
  ASSERT_TRUE(res_b.is_ok());
  zx::interrupt dev_a_irq = std::move(res_a.value());
  zx::interrupt dev_b_irq = std::move(res_b.value());

  zx::port port;
  ASSERT_OK(zx::port::create(ZX_PORT_BIND_TO_INTERRUPT, &port));
  ASSERT_OK(dev_a_irq.bind(port, 1, 0));
  ASSERT_OK(dev_b_irq.bind(port, 2, 0));

  // Under workaround, both devices are signaled and masked on vector trigger.
  ASSERT_OK(bus_interrupt.trigger(0, zx::clock::get_boot()));
  driver_test().runtime().RunUntilIdle();
  zx_port_packet_t packet;
  ASSERT_OK(port.wait(zx::time::infinite(), &packet));
  EXPECT_TRUE(packet.key == 1u || packet.key == 2u);
  uint64_t first_key = packet.key;
  ASSERT_OK(port.wait(zx::time::infinite(), &packet));
  EXPECT_TRUE(packet.key == 1u || packet.key == 2u);
  EXPECT_NE(packet.key, first_key);
  ASSERT_TRUE(IsLegacyIrqDisabled(bus_dev_a));
  ASSERT_TRUE(IsCommandIntDisabled(device_a));
  ASSERT_TRUE(IsLegacyIrqDisabled(bus_dev_b));
  ASSERT_TRUE(IsCommandIntDisabled(device_b));

  // Acknowledge Device A only -> Device A unmasks.
  ASSERT_OK(dev_a_irq.ack());
  driver_test().runtime().RunUntilIdle();
  ASSERT_FALSE(IsLegacyIrqDisabled(bus_dev_a));
  ASSERT_FALSE(IsCommandIntDisabled(device_a));
  ASSERT_TRUE(IsLegacyIrqDisabled(bus_dev_b));
  ASSERT_TRUE(IsCommandIntDisabled(device_b));

  // Trigger vector again -> Device A is signaled and masked; Device B (pending) is skipped.
  ASSERT_OK(bus_interrupt.trigger(0, zx::clock::get_boot()));
  driver_test().runtime().RunUntilIdle();
  ASSERT_OK(port.wait(zx::time::infinite(), &packet));
  EXPECT_EQ(packet.key, 1u);
  ASSERT_TRUE(IsLegacyIrqDisabled(bus_dev_a));
  ASSERT_TRUE(IsCommandIntDisabled(device_a));
  ASSERT_TRUE(IsLegacyIrqDisabled(bus_dev_b));
  ASSERT_TRUE(IsCommandIntDisabled(device_b));

  // Acknowledge both devices to return them to unmasked state.
  ASSERT_OK(dev_a_irq.ack());
  ASSERT_OK(dev_b_irq.ack());
  driver_test().runtime().RunUntilIdle();
  ASSERT_FALSE(IsLegacyIrqDisabled(bus_dev_a));
  ASSERT_FALSE(IsCommandIntDisabled(device_a));
  ASSERT_FALSE(IsLegacyIrqDisabled(bus_dev_b));
  ASSERT_FALSE(IsCommandIntDisabled(device_b));
}

TEST_F(PciBusTests, LegacyIrqHandlerErrorAndEdgeCases) {
  fuchsia_hardware_pci::wire::Bdf device_bdf = {.bus_id = 0, .device_id = 0, .function_id = 0};
  ecam()
      .get_device(device_bdf)
      ->set_vendor_id(0x8086)
      .set_device_id(0x8086)
      .set_interrupt_pin(0x1)
      .set_status(kStatusInterrupt);
  constexpr uint8_t kVector = 0x10;
  zx::interrupt bus_interrupt = AddLegacyIrqToBus(kVector);
  AddRoutingEntryToBus(/*p_dev=*/std::nullopt, /*p_func=*/std::nullopt, /*dev_id=*/0,
                       /*a=*/kVector, /*b=*/0, /*c=*/0, /*d=*/0);

  ASSERT_OK(StartDriver());
  auto* bus = this->bus();
  auto* bus_device = bus->GetDevice(device_bdf);
  ASSERT_OK(bus_device->SetIrqMode(fuchsia_hardware_pci::InterruptMode::kLegacy, 1));

  // HandleLegacyIrq with ZX_ERR_CANCELED returns early.
  bus->HandleLegacyIrq(nullptr, nullptr, ZX_ERR_CANCELED, nullptr, kVector);

  // HandleLegacyIrq with non-OK status logs error and returns early.
  bus->HandleLegacyIrq(nullptr, nullptr, ZX_ERR_INTERNAL, nullptr, kVector);

  // HandleLegacyIrq with null interrupt packet logs error and returns early.
  bus->HandleLegacyIrq(nullptr, nullptr, ZX_OK, nullptr, kVector);

  // HandleLegacyIrq with untracked vector logs error and returns early.
  zx_packet_interrupt_t packet = {};
  bus->HandleLegacyIrq(nullptr, nullptr, ZX_OK, &packet, 0x99);

  // HandleDeviceLegacyIrqUntriggered with ZX_ERR_CANCELED returns early.
  bus->HandleDeviceLegacyIrqUntriggered(nullptr, nullptr, ZX_ERR_CANCELED, nullptr, bus_device);

  // HandleDeviceLegacyIrqUntriggered with non-OK status logs error and returns early.
  bus->HandleDeviceLegacyIrqUntriggered(nullptr, nullptr, ZX_ERR_INTERNAL, nullptr, bus_device);

  // Invalidate the device's virtual interrupt handle so SignalLegacyIrq fails.
  // Device remains masked to prevent interrupt storms, and ack failure on
  // the untriggered bus_interrupt is safely handled.
  {
    fbl::AutoLock _(bus_device->dev_lock());
    bus_device->irqs().legacy.reset();
  }
  packet.timestamp = zx::clock::get_boot().get();
  bus->HandleLegacyIrq(nullptr, nullptr, ZX_OK, &packet, kVector);
  {
    fbl::AutoLock _(bus_device->dev_lock());
    EXPECT_TRUE(bus_device->irqs().legacy_disabled);
  }

  // HandleDeviceLegacyIrqUntriggered returns early if mode is not kLegacy.
  ASSERT_OK(bus_device->SetIrqMode(fuchsia_hardware_pci::InterruptMode::kDisabled, 0));
  bus->HandleDeviceLegacyIrqUntriggered(nullptr, nullptr, ZX_OK, nullptr, bus_device);
  {
    fbl::AutoLock _(bus_device->dev_lock());
    EXPECT_TRUE(bus_device->irqs().legacy_disabled);
    EXPECT_EQ(bus_device->irqs().mode, fuchsia_hardware_pci::InterruptMode::kDisabled);
  }
}

TEST_F(PciBusTests, ObeysHeaderTypeMultiFn) {
  ecam()
      .get_device({.bus_id = 0, .device_id = 0, .function_id = 0})
      ->set_vendor_id(0x8086)
      .set_device_id(1)
      .set_header_type(config::HeaderType.FromValue(0).set_multi_function(1).reg_value());
  ecam()
      .get_device({.bus_id = 0, .device_id = 0, .function_id = 1})
      ->set_vendor_id(0x8086)
      .set_device_id(2);
  ecam()
      .get_device({.bus_id = 0, .device_id = 1, .function_id = 0})
      ->set_vendor_id(0x8086)
      .set_device_id(3)
      .set_header_type(0);
  ecam()
      .get_device({.bus_id = 0, .device_id = 1, .function_id = 1})
      ->set_vendor_id(0x8086)
      .set_device_id(4);

  ASSERT_OK(StartDriver());
  auto* bus = this->bus();
  ASSERT_EQ(bus->GetDeviceCount(), 3u);
}

TEST_F(PciBusTests, MultiFnAllFunctionsScanned) {
  uint32_t dev_cnt = SetupMultifunctionTopology();
  ASSERT_OK(StartDriver());
  auto* bus = this->bus();
  ASSERT_EQ(bus->GetDeviceCount(), dev_cnt);
}

// A device whose BDF the platform reports as devicetree-described gets only a
// fragment from the bus driver; the bus driver does not publish a composite node
// spec for it (the devicetree owns the composite).
TEST_F(PciBusTests, DevicetreeDeviceSkipsCompositeSpec) {
  ecam()
      .get_device({.bus_id = 0, .device_id = 0, .function_id = 0})
      ->set_vendor_id(0x8086)
      .set_device_id(1);
  AddDevicetreeDevice(fuchsia_hardware_pci::Bdf{{.bus_id = 0, .device_id = 0, .function_id = 0}});

  ASSERT_OK(StartDriver());
  auto* bus = this->bus();
  ASSERT_EQ(bus->GetDeviceCount(), 1u);
  EXPECT_EQ(GetCompositeSpecs().size(), 0u);
}

// The same topology without the devicetree entry: the bus driver publishes a
// composite node spec as usual.
TEST_F(PciBusTests, NonDevicetreeDeviceAddsCompositeSpec) {
  ecam()
      .get_device({.bus_id = 0, .device_id = 0, .function_id = 0})
      ->set_vendor_id(0x8086)
      .set_device_id(1);

  ASSERT_OK(StartDriver());
  auto* bus = this->bus();
  ASSERT_EQ(bus->GetDeviceCount(), 1u);
  auto specs = GetCompositeSpecs();
  ASSERT_EQ(specs.size(), 1u);
  const auto& parents = specs[0].parents2();
  ASSERT_TRUE(parents.has_value());
  EXPECT_EQ(parents->size(), 1u);
}

TEST_F(PciBusTests, AcpiDeviceCompositeSpecIncludesAcpiParent) {
  ecam()
      .get_device({.bus_id = 0, .device_id = 0, .function_id = 0})
      ->set_vendor_id(0x8086)
      .set_device_id(1);
  AddAcpiDevice(fuchsia_hardware_pci::Bdf{{.bus_id = 0, .device_id = 0, .function_id = 0}});

  ASSERT_OK(StartDriver());
  ASSERT_EQ(bus()->GetDeviceCount(), 1u);
  auto specs = GetCompositeSpecs();
  ASSERT_EQ(specs.size(), 1u);
  const auto& parents = specs[0].parents2();
  ASSERT_TRUE(parents.has_value());
  EXPECT_EQ(parents->size(), 2u);
}

// A composite spec rejected with a domain error leaves the device and its fragment node in place.
TEST_F(PciBusTests, CompositeNodeSpecDomainErrorKeepsDevice) {
  ecam()
      .get_device({.bus_id = 0, .device_id = 0, .function_id = 0})
      ->set_vendor_id(0x8086)
      .set_device_id(1);
  driver_test().RunInEnvironmentTypeContext([](PciBusTestEnvironment& env) {
    env.composite_manager().set_status(
        fuchsia_driver_framework::CompositeNodeSpecError::kDriverIndexFailure);
  });

  ASSERT_OK(StartDriver());
  EXPECT_EQ(bus()->GetDeviceCount(), 1u);
  size_t child_count = driver_test().RunInNodeContext<size_t>([](fdf_testing::TestNode& node) {
    return node.children().at("bus").children().count("00_00_0");
  });
  EXPECT_EQ(child_count, 1u);
}

// A composite spec rejected with a framework error removes the node and drops the device.
TEST_F(PciBusTests, CompositeNodeSpecFrameworkErrorRemovesDevice) {
  ecam()
      .get_device({.bus_id = 0, .device_id = 0, .function_id = 0})
      ->set_vendor_id(0x8086)
      .set_device_id(1);
  driver_test().RunInEnvironmentTypeContext(
      [](PciBusTestEnvironment& env) { env.composite_manager().set_epitaph(ZX_ERR_INTERNAL); });

  ASSERT_OK(StartDriver());
  EXPECT_EQ(bus()->GetDeviceCount(), 0u);

  driver_test().runtime().RunUntil([&]() {
    return driver_test().RunInNodeContext<size_t>([](fdf_testing::TestNode& node) {
      return node.children().at("bus").children().count("00_00_0");
    }) == 0u;
  });

  zx::result device_client_end =
      driver_test().Connect<fuchsia_hardware_pci::Service::Device>("00_00_0");
  ASSERT_OK(device_client_end);
  zx::result run_result = driver_test().RunOnBackgroundDispatcherSync([&]() {
    fidl::SyncClient client(std::move(*device_client_end));
    auto info_result = client->GetDeviceInfo();
    EXPECT_TRUE(info_result.is_error());
    EXPECT_TRUE(info_result.error_value().is_peer_closed());
  });
  ASSERT_OK(run_result);
}

TEST_F(PciBusTests, CompositeNodeManagerAlreadyExistsSucceeds) {
  ecam()
      .get_device({.bus_id = 0, .device_id = 0, .function_id = 0})
      ->set_vendor_id(0x8086)
      .set_device_id(1);
  driver_test().RunInEnvironmentTypeContext([](PciBusTestEnvironment& env) {
    env.composite_manager().set_status(
        fuchsia_driver_framework::CompositeNodeSpecError::kAlreadyExists);
  });

  ASSERT_OK(StartDriver());
  EXPECT_EQ(bus()->GetDeviceCount(), 1u);
  size_t child_count = driver_test().RunInNodeContext<size_t>([](fdf_testing::TestNode& node) {
    return node.children().at("bus").children().count("00_00_0");
  });
  EXPECT_EQ(child_count, 1u);
}

TEST_F(PciBusTests, MetadataEmpty) {
  ASSERT_OK(StartDriver());
  EXPECT_TRUE(bus()->GetBoardConfiguration().IsEmpty());
}

TEST_F(PciBusTests, MetadataPopulated) {
  driver_test().RunInEnvironmentTypeContext([](PciBusTestEnvironment& env) {
    env.SetMetadata({{.use_intx_workaround = PciFidl::UseIntxWorkaroundType()}});
  });
  ASSERT_OK(StartDriver());
  EXPECT_FALSE(bus()->GetBoardConfiguration().IsEmpty());
  EXPECT_TRUE(bus()->GetBoardConfiguration().use_intx_workaround().has_value());
}

TEST_F(PciBusTests, ScanBusInvalidBridgeDoesNotCrash) {
  ecam()
      .get_bridge({.bus_id = 0, .device_id = 1, .function_id = 0})
      ->set_vendor_id(0x8086)
      .set_device_id(1)
      .set_header_type(kHeaderTypePciBridge)
      .set_primary_bus_number(0)
      .set_secondary_bus_number(0);
  ecam()
      .get_device({.bus_id = 0, .device_id = 2, .function_id = 0})
      ->set_vendor_id(0x8086)
      .set_device_id(2);

  ASSERT_OK(StartDriver());
  auto* bus = this->bus();
  ASSERT_EQ(bus->GetDeviceCount(), 1u);
  EXPECT_NE(bus->GetDevice({.bus_id = 0, .device_id = 2, .function_id = 0}), nullptr);
  EXPECT_EQ(GetCompositeSpecs().size(), 1u);
}

TEST_F(PciBusTests, BridgeWindowsSurviveBusRestart) {
  ecam()
      .get_bridge({.bus_id = 0, .device_id = 1, .function_id = 0})
      ->set_vendor_id(0x8086)
      .set_device_id(1)
      .set_header_type(kHeaderTypePciBridge)
      .set_io_base(0x10)
      .set_io_limit(0xF0)
      .set_memory_base(0x1000)
      .set_memory_limit(0xBFFF)
      .set_primary_bus_number(0)
      .set_secondary_bus_number(1)
      .set_subordinate_bus_number(1);
  ecam()
      .get_device({.bus_id = 1, .device_id = 0, .function_id = 0})
      ->set_vendor_id(0x8086)
      .set_device_id(2);

  {
    ASSERT_OK(StartDriver());
    ASSERT_EQ(bus()->GetDeviceCount(), 2u);
    EXPECT_TRUE(ecam().get_bridge({.bus_id = 0, .device_id = 1, .function_id = 0})->mem_space_en());
    EXPECT_TRUE(ecam().get_bridge({.bus_id = 0, .device_id = 1, .function_id = 0})->io_space_en());
    StopAndDestroyDriver();
  }

  // Verify that Bridge::Disable() disabled command decoding without wiping out
  // the pre-configured bridge window registers in hardware config space.
  EXPECT_FALSE(ecam().get_bridge({.bus_id = 0, .device_id = 1, .function_id = 0})->mem_space_en());
  EXPECT_FALSE(ecam().get_bridge({.bus_id = 0, .device_id = 1, .function_id = 0})->io_space_en());
  EXPECT_FALSE(ecam().get_bridge({.bus_id = 0, .device_id = 1, .function_id = 0})->bus_master_en());
  EXPECT_EQ(ecam().get_bridge({.bus_id = 0, .device_id = 1, .function_id = 0})->io_base(), 0x10u);
  EXPECT_EQ(ecam().get_bridge({.bus_id = 0, .device_id = 1, .function_id = 0})->io_limit(), 0xF0u);
  EXPECT_EQ(ecam().get_bridge({.bus_id = 0, .device_id = 1, .function_id = 0})->memory_base(),
            0x1000u);
  EXPECT_EQ(ecam().get_bridge({.bus_id = 0, .device_id = 1, .function_id = 0})->memory_limit(),
            0xBFFFu);

  // Initializing a second bus against the same ECAM must parse the preserved
  // bridge windows and re-enable bridge decoding (PCI_COMMAND_MEM_EN / PCI_COMMAND_IO_EN).
  ASSERT_OK(StartDriver());
  ASSERT_EQ(bus()->GetDeviceCount(), 2u);
  EXPECT_TRUE(ecam().get_bridge({.bus_id = 0, .device_id = 1, .function_id = 0})->mem_space_en());
  EXPECT_TRUE(ecam().get_bridge({.bus_id = 0, .device_id = 1, .function_id = 0})->io_space_en());
  auto* bridge =
      static_cast<pci::Bridge*>(bus()->GetDevice({.bus_id = 0, .device_id = 1, .function_id = 0}));
  ASSERT_NE(bridge, nullptr);
  EXPECT_EQ(bridge->io_base(), 0x1000u);
  EXPECT_EQ(bridge->io_limit(), 0xFFFFu);
  EXPECT_EQ(bridge->mem_base(), 0x10000000u);
  EXPECT_EQ(bridge->mem_limit(), 0xBFFFFFFFu);
}

}  // namespace
}  // namespace pci
