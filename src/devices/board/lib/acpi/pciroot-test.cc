// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.pci.root/cpp/driver/wire.h>
#include <fidl/fuchsia.hardware.pci.root/cpp/wire.h>
#include <lib/async/cpp/task.h>
#include <lib/component/incoming/cpp/directory.h>
#include <lib/component/incoming/cpp/service.h>
#include <lib/ddk/device.h>
#include <lib/driver/incoming/cpp/namespace.h>
#include <lib/fake-msi/msi.h>
#include <lib/fake-resource/resource.h>
#include <lib/fdf/cpp/arena.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/resource.h>
#include <lib/zx/vmo.h>
#include <zircon/syscalls.h>

#include <memory>
#include <string_view>

#include <bind/fuchsia/cpp/bind.h>
#include <zxtest/zxtest.h>

#include "src/devices/board/lib/acpi/pci-internal.h"
#include "src/devices/testing/mock-ddk/mock-device.h"

namespace {

namespace fpr = fuchsia_hardware_pci_root;
namespace fpci = fuchsia_hardware_pci;

constexpr uint32_t kLegacyVector = 0x10;
constexpr uint64_t kWindowBase = 0x8000'0000;
constexpr uint64_t kWindowSize = 0x1000'0000;

class AcpiPciRootTest : public zxtest::Test {
 protected:
  void SetUp() override {
    ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_MMIO, mmio_.reset_and_get_address()));
    ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_IOPORT, ioport_.reset_and_get_address()));
    ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_SYSTEM, msi_.reset_and_get_address()));
  }

  void TearDown() override {
    for (auto& child : mock_root_->children()) {
      device_async_remove(child.get());
    }
    ASSERT_OK(mock_ddk::ReleaseFlaggedDevices(mock_root_.get(), dispatcher_->async_dispatcher()));
    mock_ddk::GetDriverRuntime()->ShutdownBackgroundDispatcher(dispatcher_->get(), [] {});
  }

  // Returns the info for a root with one legacy interrupt, one routing entry, one ACPI BDF, one
  // MMIO window, and no ECAM.
  acpi::PciRootInfo MakeInfo() {
    acpi::PciRootInfo info;
    memcpy(info.name, "PCI0", 4);
    info.end_bus_num = 0x7f;
    info.msi_resource = msi_.borrow();

    zx::interrupt interrupt;
    EXPECT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));
    info.legacy_irqs.emplace(kLegacyVector, std::move(interrupt));
    info.routing.push_back({
        .port_device_id = fpr::wire::kIrqRoutingNoParent,
        .port_function_id = fpr::wire::kIrqRoutingNoParent,
        .device_id = 3,
        .pins = {kLegacyVector, 0, 0, 0},
    });
    info.acpi_bdfs.push_back({.bus_id = 0, .device_id = 1, .function_id = 2});

    info.windows = std::make_unique<acpi::PciRootWindows>(mmio_.borrow(), ioport_.borrow());
    EXPECT_OK(
        info.windows->AddWindow(acpi::PciRootWindows::Space::kMemory, kWindowBase, kWindowSize));
    return info;
  }

  // Publishes |info| the way the board driver does.
  void Publish(acpi::PciRootInfo info) {
    libsync::Completion done;
    zx_status_t status = ZX_OK;
    async::PostTask(dispatcher_->async_dispatcher(), [&] {
      status =
          AcpiPciRoot::Create(std::move(info), mock_root_.get(), dispatcher_->async_dispatcher());
      done.Signal();
    });
    done.Wait();
    ASSERT_OK(status);
  }

  MockDevice* published() { return mock_root_->GetLatestChild(); }

  fidl::WireSyncClient<fpr::Root> ConnectRoot() {
    zx::result svc = component::OpenDirectoryAt(published()->outgoing(), "svc");
    EXPECT_OK(svc);
    zx::result client = component::ConnectAtMember<fpr::RootService::Root>(*svc);
    EXPECT_OK(client);
    return fidl::WireSyncClient(std::move(*client));
  }

  fdf::WireSyncClient<fpr::Config> ConnectConfig() {
    zx::result svc = component::OpenDirectoryAt(published()->outgoing(), "svc");
    EXPECT_OK(svc);
    zx::result client = fdf::internal::DriverTransportConnect<fpr::ConfigService::Config>(
        *svc, component::kDefaultInstance);
    EXPECT_OK(client);
    return fdf::WireSyncClient(std::move(*client));
  }

  std::shared_ptr<MockDevice> mock_root_ = MockDevice::FakeRootParent();
  fdf::UnownedSynchronizedDispatcher dispatcher_ =
      mock_ddk::GetDriverRuntime()->StartBackgroundDispatcher();
  zx::resource mmio_;
  zx::resource ioport_;
  zx::resource msi_;
};

TEST_F(AcpiPciRootTest, PublishedWithRootServiceProperty) {
  ASSERT_NO_FATAL_FAILURE(Publish(MakeInfo()));

  bool found_service = false;
  for (const zx_device_str_prop_t& prop : published()->GetStringProperties()) {
    if (std::string_view(prop.key) == bind_fuchsia::SERVICE) {
      EXPECT_EQ(std::string_view(prop.property_value.data.str_val), fpr::RootService::Name);
      found_service = true;
    }
  }
  EXPECT_TRUE(found_service);
}

TEST_F(AcpiPciRootTest, GetPlatformInfo) {
  ASSERT_NO_FATAL_FAILURE(Publish(MakeInfo()));

  fidl::WireSyncClient root = ConnectRoot();
  fidl::WireResult result = root->GetPlatformInfo();
  ASSERT_OK(result.status());
  ASSERT_TRUE(result->is_ok());
  const fpr::wire::PlatformInfo& info = result->value()->info;
  EXPECT_EQ(info.name.get(), "PCI0");
  EXPECT_EQ(info.start_bus_num, 0);
  EXPECT_EQ(info.end_bus_num, 0x7f);
  EXPECT_EQ(info.segment_group, 0);

  ASSERT_EQ(info.irq_routing.size(), 1u);
  EXPECT_EQ(info.irq_routing[0].port_device_id, fpr::wire::kIrqRoutingNoParent);
  EXPECT_EQ(info.irq_routing[0].device_id, 3);
  EXPECT_EQ(info.irq_routing[0].pins[0], kLegacyVector);

  ASSERT_TRUE(info.parent_bdfs.has_value());
  ASSERT_TRUE(info.parent_bdfs->is_acpi());
  ASSERT_EQ(info.parent_bdfs->acpi().size(), 1u);
  EXPECT_EQ(info.parent_bdfs->acpi()[0].device_id, 1);
  EXPECT_EQ(info.parent_bdfs->acpi()[0].function_id, 2);
}

TEST_F(AcpiPciRootTest, GetPlatformInfoWithoutAcpiBdfs) {
  acpi::PciRootInfo info = MakeInfo();
  info.acpi_bdfs.clear();
  ASSERT_NO_FATAL_FAILURE(Publish(std::move(info)));

  fidl::WireSyncClient root = ConnectRoot();
  fidl::WireResult result = root->GetPlatformInfo();
  ASSERT_OK(result.status());
  ASSERT_TRUE(result->is_ok());
  EXPECT_FALSE(result->value()->info.parent_bdfs.has_value());
}

TEST_F(AcpiPciRootTest, GetCamWithoutEcam) {
  ASSERT_NO_FATAL_FAILURE(Publish(MakeInfo()));

  fidl::WireSyncClient root = ConnectRoot();
  fidl::WireResult cam = root->GetCam();
  ASSERT_OK(cam.status());
  ASSERT_TRUE(cam->is_error());
  EXPECT_STATUS(cam->error_value(), ZX_ERR_NOT_SUPPORTED);
}

TEST_F(AcpiPciRootTest, GetCamWithEcam) {
  acpi::PciRootInfo info = MakeInfo();
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &info.ecam.emplace()));
  ASSERT_NO_FATAL_FAILURE(Publish(std::move(info)));

  fidl::WireSyncClient root = ConnectRoot();
  fidl::WireResult ecam_cam = root->GetCam();
  ASSERT_OK(ecam_cam.status());
  ASSERT_TRUE(ecam_cam->is_ok());
  EXPECT_TRUE(ecam_cam->value()->vmo.is_valid());
  EXPECT_TRUE(ecam_cam->value()->is_extended);

  fdf::Arena arena('PCIR');
  const fpci::wire::Bdf bdf = {.bus_id = 0, .device_id = 1, .function_id = 0};
  fdf::WireSyncClient config = ConnectConfig();
  auto read_closed = config.buffer(arena)->Read8(bdf, 0);
  EXPECT_FALSE(read_closed.ok());
  EXPECT_STATUS(read_closed.status(), ZX_ERR_PEER_CLOSED);
}

TEST_F(AcpiPciRootTest, ConfigValidatesBdfAndHeaderBounds) {
  ASSERT_NO_FATAL_FAILURE(Publish(MakeInfo()));

  fdf::WireSyncClient config = ConnectConfig();
  const fpci::wire::Bdf bdf = {.bus_id = 0, .device_id = 1, .function_id = 0};
  fdf::Arena arena('PCIR');

  auto read_oob = config.buffer(arena)->Read32(bdf, 0xfe);
  ASSERT_OK(read_oob.status());
  ASSERT_TRUE(read_oob->is_error());
  EXPECT_STATUS(read_oob->error_value(), ZX_ERR_OUT_OF_RANGE);

  auto write_oob = config.buffer(arena)->Write8(bdf, 0x100, 0);
  ASSERT_OK(write_oob.status());
  ASSERT_TRUE(write_oob->is_error());
  EXPECT_STATUS(write_oob->error_value(), ZX_ERR_OUT_OF_RANGE);

  auto bad_dev = config.buffer(arena)->Read8({.bus_id = 0, .device_id = 32, .function_id = 0}, 0);
  ASSERT_OK(bad_dev.status());
  ASSERT_TRUE(bad_dev->is_error());
  EXPECT_STATUS(bad_dev->error_value(), ZX_ERR_INVALID_ARGS);

  auto bad_func =
      config.buffer(arena)->Write8({.bus_id = 0, .device_id = 1, .function_id = 8}, 0, 0);
  ASSERT_OK(bad_func.status());
  ASSERT_TRUE(bad_func->is_error());
  EXPECT_STATUS(bad_func->error_value(), ZX_ERR_INVALID_ARGS);
}

TEST_F(AcpiPciRootTest, ConfigNotServedForNonZeroSegment) {
  acpi::PciRootInfo info = MakeInfo();
  info.segment_group = 1;
  ASSERT_NO_FATAL_FAILURE(Publish(std::move(info)));

  fdf::Arena arena('PCIR');
  const fpci::wire::Bdf bdf = {.bus_id = 0, .device_id = 1, .function_id = 0};
  fdf::WireSyncClient config = ConnectConfig();
  auto read_closed = config.buffer(arena)->Read8(bdf, 0);
  EXPECT_FALSE(read_closed.ok());
  EXPECT_STATUS(read_closed.status(), ZX_ERR_PEER_CLOSED);
}

TEST_F(AcpiPciRootTest, GetLegacyInterrupt) {
  ASSERT_NO_FATAL_FAILURE(Publish(MakeInfo()));
  fidl::WireSyncClient root = ConnectRoot();

  fidl::WireResult legacy = root->GetLegacyInterrupt(kLegacyVector);
  ASSERT_OK(legacy.status());
  ASSERT_TRUE(legacy->is_ok());
  EXPECT_TRUE(legacy->value()->interrupt.is_valid());

  fidl::WireResult missing = root->GetLegacyInterrupt(kLegacyVector + 1);
  ASSERT_OK(missing.status());
  ASSERT_TRUE(missing->is_error());
  EXPECT_STATUS(missing->error_value(), ZX_ERR_NOT_FOUND);
}

TEST_F(AcpiPciRootTest, GetAddressSpace) {
  ASSERT_NO_FATAL_FAILURE(Publish(MakeInfo()));
  fidl::WireSyncClient root = ConnectRoot();

  fidl::WireResult space = root->GetAddressSpace(kWindowBase, zx_system_get_page_size(),
                                                 fpr::wire::AddressSpace::kMemory, /*low=*/true);
  ASSERT_OK(space.status());
  ASSERT_TRUE(space->is_ok());
  EXPECT_EQ(space->value()->base, kWindowBase);
  EXPECT_TRUE(space->value()->resource.is_valid());
  EXPECT_TRUE(space->value()->token.is_valid());

  fidl::WireResult io = root->GetAddressSpace(0, 0x10, fpr::wire::AddressSpace::kIo, /*low=*/false);
  ASSERT_OK(io.status());
  ASSERT_TRUE(io->is_error());
  EXPECT_STATUS(io->error_value(), ZX_ERR_NOT_FOUND);
}

TEST_F(AcpiPciRootTest, GetBti) {
  ASSERT_NO_FATAL_FAILURE(Publish(MakeInfo()));
  fidl::WireSyncClient root = ConnectRoot();

  fidl::WireResult bad_index =
      root->GetBti({.bus_id = 0, .device_id = 1, .function_id = 0}, /*index=*/1);
  ASSERT_OK(bad_index.status());
  ASSERT_TRUE(bad_index->is_error());
  EXPECT_STATUS(bad_index->error_value(), ZX_ERR_OUT_OF_RANGE);

  fidl::WireResult bad_dev =
      root->GetBti({.bus_id = 0, .device_id = 32, .function_id = 0}, /*index=*/0);
  ASSERT_OK(bad_dev.status());
  ASSERT_TRUE(bad_dev->is_error());
  EXPECT_STATUS(bad_dev->error_value(), ZX_ERR_INVALID_ARGS);

  fidl::WireResult bad_func =
      root->GetBti({.bus_id = 0, .device_id = 1, .function_id = 8}, /*index=*/0);
  ASSERT_OK(bad_func.status());
  ASSERT_TRUE(bad_func->is_error());
  EXPECT_STATUS(bad_func->error_value(), ZX_ERR_INVALID_ARGS);

  fidl::WireResult bad_handle =
      root->GetBti({.bus_id = 1, .device_id = 2, .function_id = 3}, /*index=*/0);
  ASSERT_OK(bad_handle.status());
  ASSERT_TRUE(bad_handle->is_error());
  EXPECT_STATUS(bad_handle->error_value(), ZX_ERR_BAD_HANDLE);
}

TEST_F(AcpiPciRootTest, AllocateMsiAndGetMsiHandle) {
  ASSERT_NO_FATAL_FAILURE(Publish(MakeInfo()));
  fidl::WireSyncClient root = ConnectRoot();

  fidl::WireResult alloc = root->AllocateMsi(1, /*can_target_64bit=*/true);
  ASSERT_OK(alloc.status());
  ASSERT_TRUE(alloc->is_ok());
  EXPECT_TRUE(alloc->value()->allocation.is_valid());
  EXPECT_EQ(alloc->value()->info.irq_count, 1u);

  zx::handle msi_handle = std::move(alloc->value()->allocation);
  zx::handle msi_keepalive;
  ASSERT_OK(msi_handle.duplicate(ZX_RIGHT_SAME_RIGHTS, &msi_keepalive));

  zx::vmo cfg_vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &cfg_vmo));
  ASSERT_OK(cfg_vmo.set_cache_policy(ZX_CACHE_POLICY_UNCACHED_DEVICE));

  fidl::WireResult msi_handle_result =
      root->GetMsiHandle(std::move(msi_handle), /*options=*/0, /*msi_id=*/0, std::move(cfg_vmo),
                         /*cfg_offset=*/0);
  ASSERT_OK(msi_handle_result.status());
  ASSERT_TRUE(msi_handle_result->is_ok());
  EXPECT_TRUE(msi_handle_result->value()->interrupt.is_valid());
}

}  // namespace
