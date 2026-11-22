// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/aml-canvas/aml-canvas-driver.h"

#include <fidl/fuchsia.driver.metadata/cpp/fidl.h>
#include <fidl/fuchsia.hardware.amlogiccanvas/cpp/wire.h>
#include <lib/driver/compat/cpp/logging.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/testing/cpp/driver_test.h>

#include <bind/fuchsia/cpp/bind.h>
#include <gtest/gtest.h>

#include "src/lib/testing/predicates/status.h"

namespace aml_canvas {

namespace {

class AmlCanvasDriverTestEnvironment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();

    zx::vmo mmio_vmo;
    static constexpr uint64_t kMmioVmoSize = 0x2000;
    zx_status_t status = zx::vmo::create(kMmioVmoSize, 0, &mmio_vmo);
    if (status != ZX_OK) {
      return zx::error(status);
    }

    fdf_fake::FakePDev::Config config;
    config.use_fake_bti = true;
    config.mmios[0] = fdf::PDev::MmioInfo{
        .offset = 0,
        .size = kMmioVmoSize,
        .vmo = std::move(mmio_vmo),
    };
    fake_pdev_.SetConfig(std::move(config));

    auto instance_handler = fake_pdev_.GetInstanceHandler(dispatcher);
    return to_driver_vfs.AddService<fuchsia_hardware_platform_device::Service>(
        std::move(instance_handler));
  }

  fdf_fake::FakePDev& fake_pdev() { return fake_pdev_; }

 private:
  fdf_fake::FakePDev fake_pdev_;
};

class TestConfig final {
 public:
  using DriverType = AmlCanvasDriver;
  using EnvironmentType = AmlCanvasDriverTestEnvironment;
};

class AmlCanvasDriverTest : public ::testing::Test {
 public:
  void SetUp() override {
    zx::result<> result = driver_test().StartDriver();
    ASSERT_OK(result);
  }

  void TearDown() override {
    zx::result<> result = driver_test().StopDriver();
    ASSERT_OK(result);
  }

  fdf_testing::BackgroundDriverTest<TestConfig>& driver_test() { return driver_test_; }

 private:
  fdf_testing::BackgroundDriverTest<TestConfig> driver_test_;
};

TEST_F(AmlCanvasDriverTest, Lifecycle) {
  // SetUp and TearDown handle start and stop.
}

TEST_F(AmlCanvasDriverTest, ServesAmlogicCanvasDeviceProtocol) {
  zx::result canvas_client_end =
      driver_test().Connect<fuchsia_hardware_amlogiccanvas::Service::Device>();
  ASSERT_OK(canvas_client_end);
}

TEST_F(AmlCanvasDriverTest, SingleChildWithoutDmlMetadata) {
  driver_test().RunInNodeContext([](fdf_testing::TestNode& node) {
    ASSERT_EQ(node.children().size(), 1u);
    fdf_testing::TestNode& child = node.children().at("aml-canvas");
    EXPECT_TRUE(child.children().empty());
    std::vector<fuchsia_driver_framework::NodeProperty2> properties = child.GetProperties();
    ASSERT_EQ(properties.size(), 1u);
    EXPECT_EQ(properties[0].key(), bind_fuchsia::SERVICE);
  });
}

// With DML board metadata, each canvas consumer gets its own child carrying its global ID.
TEST(AmlCanvasDriverDmlTest, ChildPerConsumer) {
  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;
  driver_test.RunInEnvironmentTypeContext([](AmlCanvasDriverTestEnvironment& env) {
    using fuchsia_driver_metadata::DictionaryValue;
    std::vector<fuchsia_driver_metadata::DictionaryEntry> entries;
    entries.emplace_back("provider", DictionaryValue::WithInt64(7));
    entries.emplace_back("consumers._count", DictionaryValue::WithInt64(2));
    entries.emplace_back("consumers.0.id", DictionaryValue::WithInt64(0x12345678));
    entries.emplace_back("consumers.0.name", DictionaryValue::WithStr("display-ff900000"));
    entries.emplace_back("consumers.1.id", DictionaryValue::WithInt64(0x9abcdef0));
    entries.emplace_back("consumers.1.name", DictionaryValue::WithStr("video-decoder-ffd00000"));
    EXPECT_OK(env.fake_pdev().AddFidlMetadata(
        "fuchsia.hardware.amlogiccanvas.Metadata",
        fuchsia_driver_metadata::Dictionary{{.entries = std::move(entries)}}));
  });
  ASSERT_OK(driver_test.StartDriver());

  driver_test.RunInNodeContext([](fdf_testing::TestNode& node) {
    ASSERT_EQ(node.children().size(), 1u);
    fdf_testing::TestNode& canvas_node = node.children().at("aml-canvas");
    EXPECT_TRUE(canvas_node.GetProperties().empty());
    ASSERT_EQ(canvas_node.children().size(), 2u);

    for (const auto& [name, id] : {std::pair<std::string, uint32_t>{"display-ff900000", 0x12345678},
                                   {"video-decoder-ffd00000", 0x9abcdef0}}) {
      SCOPED_TRACE(name);
      std::vector<fuchsia_driver_framework::NodeProperty2> properties =
          canvas_node.children().at(name).GetProperties();
      ASSERT_EQ(properties.size(), 3u);
      EXPECT_EQ(properties[0].key(), bind_fuchsia::SERVICE);
      EXPECT_EQ(properties[1].key(), bind_fuchsia::NAME);
      ASSERT_TRUE(properties[1].value().string_value().has_value());
      EXPECT_EQ(properties[1].value().string_value().value(), name);
      EXPECT_EQ(properties[2].key(), bind_fuchsia::ID);
      ASSERT_TRUE(properties[2].value().int_value().has_value());
      EXPECT_EQ(properties[2].value().int_value().value(), id);
    }
  });

  zx::result canvas_client_end =
      driver_test.Connect<fuchsia_hardware_amlogiccanvas::Service::Device>();
  ASSERT_OK(canvas_client_end);

  ASSERT_OK(driver_test.StopDriver());
}

// DML board metadata without consumers is a board configuration error.
TEST(AmlCanvasDriverDmlTest, NoConsumers) {
  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;
  driver_test.RunInEnvironmentTypeContext([](AmlCanvasDriverTestEnvironment& env) {
    std::vector<fuchsia_driver_metadata::DictionaryEntry> entries;
    entries.emplace_back("consumers._count",
                         fuchsia_driver_metadata::DictionaryValue::WithInt64(0));
    EXPECT_OK(env.fake_pdev().AddFidlMetadata(
        "fuchsia.hardware.amlogiccanvas.Metadata",
        fuchsia_driver_metadata::Dictionary{{.entries = std::move(entries)}}));
  });
  EXPECT_STATUS(driver_test.StartDriver(), ZX_ERR_INVALID_ARGS);
}

}  // namespace

}  // namespace aml_canvas
