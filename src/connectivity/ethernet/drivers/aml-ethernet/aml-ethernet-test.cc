// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "aml-ethernet.h"

#include <fidl/fuchsia.driver.metadata/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/zx/vmo.h>

#include <format>
#include <optional>
#include <vector>

#include <bind/fuchsia/cpp/bind.h>
#include <zxtest/zxtest.h>

#include "src/devices/gpio/testing/fake-gpio/fake-gpio.h"
#include "src/devices/testing/mock-ddk/mock-device.h"

namespace eth {
namespace {

constexpr uint64_t kMmioSize = 0x1000;

struct IncomingNamespace {
  fdf_fake::FakePDev pdev_server;
  fake_gpio::FakeGpio gpio_int;
  component::OutgoingDirectory pdev_outgoing{async_get_default_dispatcher()};
  component::OutgoingDirectory gpio_outgoing{async_get_default_dispatcher()};
};

class AmlEthernetTest : public zxtest::Test {
 public:
  void SetUp() override { ASSERT_OK(incoming_loop_.StartThread("incoming-ns-thread")); }

  // Serves the fragments that the driver needs. If `consumer_ids` is set, the platform device also
  // provides the metadata that DML boards generate with an entry for each ID.
  void ServeFragments(std::optional<std::vector<uint32_t>> consumer_ids) {
    fdf_fake::FakePDev::Config config;
    for (uint32_t i = 0; i < 2; ++i) {
      zx::vmo vmo;
      ASSERT_OK(zx::vmo::create(kMmioSize, 0, &vmo));
      config.mmios[i] = fdf::PDev::MmioInfo{.offset = 0, .size = kMmioSize, .vmo = std::move(vmo)};
    }

    std::optional<fuchsia_driver_metadata::Dictionary> metadata;
    if (consumer_ids.has_value()) {
      using fuchsia_driver_metadata::DictionaryValue;
      std::vector<fuchsia_driver_metadata::DictionaryEntry> entries;
      entries.emplace_back("provider", DictionaryValue::WithInt64(7));
      entries.emplace_back("consumers._count", DictionaryValue::WithInt64(consumer_ids->size()));
      for (size_t i = 0; i < consumer_ids->size(); ++i) {
        entries.emplace_back(std::format("consumers.{}.id", i),
                             DictionaryValue::WithInt64((*consumer_ids)[i]));
        entries.emplace_back(std::format("consumers.{}.name", i),
                             DictionaryValue::WithStr("ethernet-board"));
      }
      metadata.emplace(fuchsia_driver_metadata::Dictionary{{.entries = std::move(entries)}});
    }

    auto pdev_endpoints = fidl::Endpoints<fuchsia_io::Directory>::Create();
    auto gpio_endpoints = fidl::Endpoints<fuchsia_io::Directory>::Create();
    incoming_.SyncCall(
        [config = std::move(config), metadata = std::move(metadata),
         pdev_server = std::move(pdev_endpoints.server),
         gpio_server = std::move(gpio_endpoints.server)](IncomingNamespace* incoming) mutable {
          incoming->pdev_server.SetConfig(std::move(config));
          if (metadata.has_value()) {
            ASSERT_OK(incoming->pdev_server.AddFidlMetadata(
                "fuchsia.hardware.ethernet.board.Metadata", *metadata));
          }
          ASSERT_OK(incoming->pdev_outgoing.AddService<fuchsia_hardware_platform_device::Service>(
              incoming->pdev_server.GetInstanceHandler()));
          ASSERT_OK(incoming->pdev_outgoing.Serve(std::move(pdev_server)));
          ASSERT_OK(incoming->gpio_outgoing.AddService<fuchsia_hardware_gpio::Service>(
              incoming->gpio_int.CreateInstanceHandler()));
          ASSERT_OK(incoming->gpio_outgoing.Serve(std::move(gpio_server)));
        });
    ASSERT_NO_FATAL_FAILURE();

    fake_parent_->AddFidlService(fuchsia_hardware_platform_device::Service::Name,
                                 std::move(pdev_endpoints.client), "pdev");
    fake_parent_->AddFidlService(fuchsia_hardware_gpio::Service::Name,
                                 std::move(gpio_endpoints.client), "gpio-int");
  }

 protected:
  std::shared_ptr<MockDevice> fake_parent_ = MockDevice::FakeRootParent();
  async::Loop incoming_loop_{&kAsyncLoopConfigNoAttachToCurrentThread};
  async_patterns::TestDispatcherBound<IncomingNamespace> incoming_{incoming_loop_.dispatcher(),
                                                                   std::in_place};
};

TEST_F(AmlEthernetTest, NoGlobalIdWithoutDmlMetadata) {
  ASSERT_NO_FATAL_FAILURE(ServeFragments(std::nullopt));
  ASSERT_OK(AmlEthernet::Create(nullptr, fake_parent_.get()));

  MockDevice* child = fake_parent_->GetLatestChild();
  ASSERT_NOT_NULL(child);
  EXPECT_EQ(child->GetStringProperties().size(), 0u);
}

// The DML global ID of the board service's consumer is published as `fuchsia.ID`.
TEST_F(AmlEthernetTest, GlobalIdFromDmlMetadata) {
  ASSERT_NO_FATAL_FAILURE(ServeFragments(std::vector<uint32_t>{0x12345678}));
  ASSERT_OK(AmlEthernet::Create(nullptr, fake_parent_.get()));

  MockDevice* child = fake_parent_->GetLatestChild();
  ASSERT_NOT_NULL(child);
  cpp20::span<const zx_device_str_prop_t> props = child->GetStringProperties();
  ASSERT_EQ(props.size(), 1u);
  EXPECT_STREQ(props[0].key, bind_fuchsia::ID);
  EXPECT_EQ(props[0].property_value.data_type, ZX_DEVICE_PROPERTY_VALUE_INT);
  EXPECT_EQ(props[0].property_value.data.int_val, 0x12345678u);
}

// DML board metadata without consumers is a board configuration error.
TEST_F(AmlEthernetTest, NoConsumersRejected) {
  ASSERT_NO_FATAL_FAILURE(ServeFragments(std::vector<uint32_t>{}));
  EXPECT_STATUS(AmlEthernet::Create(nullptr, fake_parent_.get()), ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(fake_parent_->child_count(), 0u);
}

// A device can only carry one `fuchsia.ID`, so more than one consumer is rejected.
TEST_F(AmlEthernetTest, MultipleConsumersRejected) {
  ASSERT_NO_FATAL_FAILURE(ServeFragments(std::vector<uint32_t>{1, 2}));
  EXPECT_STATUS(AmlEthernet::Create(nullptr, fake_parent_.get()), ZX_ERR_NOT_SUPPORTED);
  EXPECT_EQ(fake_parent_->child_count(), 0u);
}

}  // namespace
}  // namespace eth
