// Copyright 2024 The Fuchsia Authors. All rights reserved.  Use of
// this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/camera/drivers/bus/aml_mipicsi/aml_mipi.h"

#include <lib/async-loop/default.h>
#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/component/incoming/cpp/service.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>

#include <fake-mmio-reg/fake-mmio-reg.h>
#include <zxtest/zxtest.h>

#include "src/devices/testing/mock-ddk/mock-device.h"

namespace camera::test {

class IncomingNamespace {
 public:
  void Init(fidl::ServerEnd<fuchsia_io::Directory> outgoing_server) {
    fdf_fake::FakePDev::Config pdev_config{.use_fake_bti = true, .use_fake_irq = true};
    pdev_config.mmios[AmlMipiDevice::kCsiPhy0] = fake_mmio_.GetMmioBuffer();
    pdev_config.mmios[AmlMipiDevice::kAphy0] = fake_mmio_.GetMmioBuffer();
    pdev_config.mmios[AmlMipiDevice::kCsiHost0] = fake_mmio_.GetMmioBuffer();
    pdev_config.mmios[AmlMipiDevice::kMipiAdap] = fake_mmio_.GetMmioBuffer();
    pdev_config.mmios[AmlMipiDevice::kHiu] = fake_mmio_.GetMmioBuffer();
    ASSERT_OK(pdev_.SetConfig(std::move(pdev_config)));
    ASSERT_OK(outgoing_.AddService<fuchsia_hardware_platform_device::Service>(
        pdev_.GetInstanceHandler()));
    ASSERT_OK(outgoing_.Serve(std::move(outgoing_server)));
  }

 private:
  fdf_fake::FakePDev pdev_;
  component::OutgoingDirectory outgoing_{async_get_default_dispatcher()};
  ddk_fake::FakeMmioRegRegion fake_mmio_{sizeof(uint32_t), 0x6000};
};

class AmlMipiTest : public zxtest::Test {
 public:
  void SetUp() override {
    ASSERT_OK(incoming_loop_.StartThread("incoming-namespace"));
    zx::result endpoints = fidl::CreateEndpoints<fuchsia_io::Directory>();
    ASSERT_OK(endpoints);
    incoming_namespace_.SyncCall(&IncomingNamespace::Init, std::move(endpoints->server));
    parent_->AddFidlService(fuchsia_hardware_platform_device::Service::Name,
                            std::move(endpoints->client), "pdev");
    ASSERT_OK(fdf::RunOnDispatcherSync(driver_dispatcher_->async_dispatcher(), [&]() {
      ASSERT_OK(AmlMipiDevice::Bind(nullptr, parent_.get()));
    }));
  }

  void TearDown() override {
    if (parent_->child_count() > 0) {
      auto* child = parent_->GetLatestChild();
      ASSERT_OK(fdf::RunOnDispatcherSync(driver_dispatcher_->async_dispatcher(),
                                         [&]() { child->ReleaseOp(); }));
    }
  }

 protected:
  std::shared_ptr<MockDevice> parent_ = MockDevice::FakeRootParent();
  fdf::UnownedSynchronizedDispatcher driver_dispatcher_ =
      fdf_testing::DriverRuntime::GetInstance()->StartBackgroundDispatcher();

  async::Loop incoming_loop_{&kAsyncLoopConfigNoAttachToCurrentThread};
  async_patterns::TestDispatcherBound<IncomingNamespace> incoming_namespace_{
      incoming_loop_.dispatcher(), std::in_place};
};

// Verify that the aml-mipi driver can bind, serve fuchsia.hardware.mipicsi.Service,
// handle Init and DeInit requests, and clean up client channels upon device release.
TEST_F(AmlMipiTest, CanBind) {
  ASSERT_NO_FATAL_FAILURE();
  ASSERT_EQ(1, parent_->child_count());
  auto* child = parent_->GetLatestChild();
  ASSERT_NOT_NULL(child);
  std::string service_path =
      std::string("svc/") +
      component::MakeServiceMemberPath<fuchsia_hardware_mipicsi::Service::Device>(
          component::kDefaultInstance);
  zx::result client_end =
      component::ConnectAt<fuchsia_hardware_mipicsi::Device>(child->outgoing(), service_path);
  ASSERT_OK(client_end);
  fidl::WireSyncClient client(std::move(*client_end));

  fuchsia_hardware_mipicsi::wire::MipiInfo mipi_info = {
      .channel = 0,
      .lanes = 2,
      .ui_value = 1000 / 720,
      .csi_version = 0,
  };
  fuchsia_hardware_mipicsi::wire::MipiAdapInfo dir_adap_info = {
      .resolution = {.x = 1920, .y = 1080},
      .format = fuchsia_hardware_mipicsi::wire::MipiImageFormat::kAmRaw10,
      .mode = fuchsia_hardware_mipicsi::wire::MipiModes::kDirMode,
      .path = fuchsia_hardware_mipicsi::wire::MipiPath::kPath0,
  };
  auto init_result = client->Init(mipi_info, dir_adap_info);
  ASSERT_TRUE(init_result.ok());
  ASSERT_TRUE(init_result->is_ok());

  fuchsia_hardware_mipicsi::wire::MipiAdapInfo dol_adap_info = dir_adap_info;
  dol_adap_info.mode = fuchsia_hardware_mipicsi::wire::MipiModes::kDolMode;
  auto dol_result = client->Init(mipi_info, dol_adap_info);
  ASSERT_TRUE(dol_result.ok());
  ASSERT_TRUE(dol_result->is_error());
  ASSERT_EQ(dol_result->error_value(), ZX_ERR_NOT_SUPPORTED);

  auto deinit_result = client->DeInit();
  ASSERT_TRUE(deinit_result.ok());
  ASSERT_TRUE(deinit_result->is_ok());

  ASSERT_OK(fdf::RunOnDispatcherSync(driver_dispatcher_->async_dispatcher(),
                                     [&]() { child->ReleaseOp(); }));
  auto closed_result = client->DeInit();
  ASSERT_FALSE(closed_result.ok());
  ASSERT_EQ(closed_result.status(), ZX_ERR_PEER_CLOSED);
}

TEST_F(AmlMipiTest, InitValidation) {
  ASSERT_NO_FATAL_FAILURE();
  ASSERT_EQ(1, parent_->child_count());
  auto* child = parent_->GetLatestChild();
  ASSERT_NOT_NULL(child);
  std::string service_path =
      std::string("svc/") +
      component::MakeServiceMemberPath<fuchsia_hardware_mipicsi::Service::Device>(
          component::kDefaultInstance);
  zx::result client_end =
      component::ConnectAt<fuchsia_hardware_mipicsi::Device>(child->outgoing(), service_path);
  ASSERT_OK(client_end);
  fidl::WireSyncClient client(std::move(*client_end));

  fuchsia_hardware_mipicsi::wire::MipiAdapInfo adap_info = {
      .resolution = {.x = 1920, .y = 1080},
      .format = fuchsia_hardware_mipicsi::wire::MipiImageFormat::kAmRaw10,
      .mode = fuchsia_hardware_mipicsi::wire::MipiModes::kDirMode,
      .path = fuchsia_hardware_mipicsi::wire::MipiPath::kPath0,
  };

  fuchsia_hardware_mipicsi::wire::MipiInfo mipi_info_0_lanes = {
      .channel = 0,
      .lanes = 0,
      .ui_value = 1,
      .csi_version = 0,
  };
  auto result_0_lanes = client->Init(mipi_info_0_lanes, adap_info);
  ASSERT_TRUE(result_0_lanes.ok());
  ASSERT_TRUE(result_0_lanes->is_error());
  ASSERT_EQ(result_0_lanes->error_value(), ZX_ERR_INVALID_ARGS);

  fuchsia_hardware_mipicsi::wire::MipiInfo mipi_info_5_lanes = {
      .channel = 0,
      .lanes = 5,
      .ui_value = 1,
      .csi_version = 0,
  };
  auto result_5_lanes = client->Init(mipi_info_5_lanes, adap_info);
  ASSERT_TRUE(result_5_lanes.ok());
  ASSERT_TRUE(result_5_lanes->is_error());
  ASSERT_EQ(result_5_lanes->error_value(), ZX_ERR_INVALID_ARGS);

  fuchsia_hardware_mipicsi::wire::MipiInfo mipi_info_overflow_ui = {
      .channel = 0,
      .lanes = 2,
      .ui_value = ((UINT32_MAX - 230) / 16) + 1,
      .csi_version = 0,
  };
  auto result_overflow_ui = client->Init(mipi_info_overflow_ui, adap_info);
  ASSERT_TRUE(result_overflow_ui.ok());
  ASSERT_TRUE(result_overflow_ui->is_error());
  ASSERT_EQ(result_overflow_ui->error_value(), ZX_ERR_INVALID_ARGS);
}

}  // namespace camera::test
