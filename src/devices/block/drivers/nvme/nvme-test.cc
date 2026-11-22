// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/drivers/nvme/nvme.h"

#include <fidl/fuchsia.storage.block/cpp/wire.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/driver/testing/cpp/minimal_compat_environment.h>
#include <lib/fake-bti/bti.h>
#include <lib/fdf/env.h>
#include <lib/fpromise/single_threaded_executor.h>
#include <lib/inspect/testing/cpp/inspect.h>

#include <atomic>
#include <limits>
#include <memory>
#include <thread>

#include <gtest/gtest.h>

#include "src/devices/block/drivers/nvme/commands/features.h"
#include "src/devices/block/drivers/nvme/commands/nvme-io.h"
#include "src/devices/block/drivers/nvme/fake/fake-admin-commands.h"
#include "src/devices/block/drivers/nvme/fake/fake-controller.h"
#include "src/devices/block/drivers/nvme/fake/fake-namespace.h"
#include "src/lib/testing/predicates/status.h"
#include "src/storage/lib/block_client/cpp/reader_writer.h"
#include "src/storage/lib/block_client/cpp/remote_block_device.h"
#include "src/storage/lib/buffer/vmoid_registry.h"

namespace nvme {

class TestNvme : public Nvme {
 public:
  // Modify to configure the behaviour of this test driver.
  static fake_nvme::FakeController controller_;
  static zx::bti last_bti_;

  TestNvme() = default;

 protected:
  zx::result<> InitResources() override {
    pci_ = fdf::Pci{};
    mmio_ = controller_.registers().GetBuffer();

    // Create a fake BTI.
    zx::bti fake_bti;
    ZX_ASSERT(fake_bti_create(fake_bti.reset_and_get_address()) == ZX_OK);
    ZX_ASSERT(fake_bti.duplicate(ZX_RIGHT_SAME_RIGHTS, &last_bti_) == ZX_OK);
    bti_ = std::move(fake_bti);

    controller_.SetNvme(this);
    return zx::ok();
  }

  zx_status_t SetInterruptMode(fuchsia_hardware_pci::InterruptMode mode,
                               uint32_t requested_irq_count) override {
    return controller_.SetInterruptMode(mode, requested_irq_count);
  }

  zx::result<fuchsia_hardware_pci::InterruptMode> ConfigureInterruptMode(
      uint32_t irq_count) override {
    return controller_.ConfigureInterruptMode(irq_count);
  }

  zx::result<zx::interrupt> MapInterrupt(uint32_t which_irq) override {
    return controller_.MapInterrupt(which_irq);
  }

  zx_status_t SetBusMastering(bool enabled) override {
    return controller_.SetBusMastering(enabled);
  }

  fake_nvme::FakeAdminCommands admin_commands_{controller_};
};

fake_nvme::FakeController TestNvme::controller_;
zx::bti TestNvme::last_bti_;

class TestConfig final {
 public:
  using DriverType = TestNvme;
  using EnvironmentType = fdf_testing::MinimalCompatEnvironment;
};

class NvmeTest : public ::testing::Test {
 public:
  void SetUp() override {
    TestNvme::controller_.Reset();
    TestNvme::last_bti_.reset();
  }

  zx::result<> StartDriverResult() {
    ZX_ASSERT(zx::event::create(0, &node_token_) == ZX_OK);
    zx::event token_copy;
    ZX_ASSERT(node_token_.duplicate(ZX_RIGHT_SAME_RIGHTS, &token_copy) == ZX_OK);

    zx::result<> result = driver_test().StartDriverWithCustomStartArgs(
        [&](fdf::DriverStartArgs& args) { args.node_token(std::move(token_copy)); });
    if (result.is_ok()) {
      driver_started_ = true;
    }
    return result;
  }

  void StartDriver() { ASSERT_OK(StartDriverResult()); }

  void TearDown() override {
    if (driver_started_) {
      zx::result<> result = driver_test().StopDriver();
      ASSERT_OK(result);
    }
  }

  fdf_testing::BackgroundDriverTest<TestConfig>& driver_test() { return driver_test_; }

  void CheckStringPropertyPrefix(const inspect::NodeValue& node, const std::string& property,
                                 const char* expected) {
    const auto* actual = node.get_property<inspect::StringPropertyValue>(property);
    EXPECT_TRUE(actual);
    if (!actual) {
      return;
    }
    EXPECT_EQ(0, strncmp(actual->value().data(), expected, strlen(expected)));
  }

  void CheckBooleanProperty(const inspect::NodeValue& node, const std::string& property,
                            bool expected) {
    const auto* actual = node.get_property<inspect::BoolPropertyValue>(property);
    EXPECT_TRUE(actual);
    if (!actual) {
      return;
    }
    EXPECT_EQ(actual->value(), expected);
  }

  void CheckUintProperty(const inspect::NodeValue& node, const std::string& property,
                         uint64_t expected) {
    const auto* actual = node.get_property<inspect::UintPropertyValue>(property);
    EXPECT_TRUE(actual);
    if (!actual) {
      return;
    }
    EXPECT_EQ(actual->value(), expected);
  }

 protected:
  fdf_testing::BackgroundDriverTest<TestConfig> driver_test_;
  zx::event node_token_;
  bool driver_started_ = false;
};

class NvmeTeardownTest : public ::testing::Test {
 public:
  void SetUp() override {
    TestNvme::controller_.Reset();
    TestNvme::last_bti_.reset();
  }

  void StartDriver() {
    ASSERT_OK(zx::event::create(0, &node_token_));
    zx::event token_copy;
    ASSERT_OK(node_token_.duplicate(ZX_RIGHT_SAME_RIGHTS, &token_copy));

    zx::result<> result = driver_test().StartDriverWithCustomStartArgs(
        [&](fdf::DriverStartArgs& args) { args.node_token(std::move(token_copy)); });
    ASSERT_OK(result);
  }

  fdf_testing::BackgroundDriverTest<TestConfig>& driver_test() { return driver_test_; }

 protected:
  fdf_testing::BackgroundDriverTest<TestConfig> driver_test_;
  zx::event node_token_;
};

TEST_F(NvmeTest, BasicTest) {
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  inspect::Inspector inspector = driver_test().RunInDriverContext<inspect::Inspector>(
      [](TestNvme& driver) { return driver.inspect(); });
  fpromise::result<inspect::Hierarchy> hierarchy =
      fpromise::run_single_threaded(inspect::ReadFromInspector(inspector));
  ASSERT_TRUE(hierarchy.is_ok());
  const auto* nvme = hierarchy.value().GetByPath({"nvme"});
  ASSERT_NE(nullptr, nvme);
  const auto* controller = nvme->GetByPath({"controller"});
  ASSERT_NE(nullptr, controller);
  CheckStringPropertyPrefix(controller->node(), "model_number",
                            fake_nvme::FakeAdminCommands::kModelNumber);
  CheckStringPropertyPrefix(controller->node(), "serial_number",
                            fake_nvme::FakeAdminCommands::kSerialNumber);
  CheckStringPropertyPrefix(controller->node(), "firmware_rev",
                            fake_nvme::FakeAdminCommands::kFirmwareRev);
  CheckBooleanProperty(controller->node(), "volatile_write_cache_enabled", true);
}

TEST_F(NvmeTest, NamespaceBlockInfo) {
  fake_nvme::FakeNamespace fake_ns;
  TestNvme::controller_.AddNamespace(1, fake_ns);
  driver_test().runtime().StartBackgroundDispatcher();

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  auto [volume_client, volume_server] = fidl::Endpoints<fuchsia_storage_block::Block>::Create();
  driver_test().RunInDriverContext([&volume_server](TestNvme& driver) mutable {
    Namespace* ns = driver.namespaces()[0].get();
    ns->ServeRequests(std::move(volume_server));
  });

  zx::result<std::unique_ptr<block_client::RemoteBlockDevice>> client =
      block_client::RemoteBlockDevice::Create(std::move(volume_client));
  ASSERT_OK(client);

  fuchsia_storage_block::wire::BlockInfo info;
  client.value()->BlockGetInfo(&info);
  EXPECT_EQ(512u, info.block_size);
  EXPECT_EQ(1024u, info.block_count);
  EXPECT_TRUE(info.flags & fuchsia_storage_block::wire::DeviceFlag::kFuaSupport);
  client.value().reset();
}

TEST_F(NvmeTest, NamespaceReadTest) {
  fake_nvme::FakeNamespace fake_ns;
  TestNvme::controller_.AddNamespace(1, fake_ns);
  TestNvme::controller_.AddIoCommand(
      IoCommandOpcode::kRead,
      [](Submission& submission, const TransactionData& data, Completion& completion) {
        completion.set_status_code_type(StatusCodeType::kGeneric)
            .set_status_code(GenericStatus::kSuccess);
      });
  driver_test().runtime().StartBackgroundDispatcher();

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  auto [volume_client, volume_server] = fidl::Endpoints<fuchsia_storage_block::Block>::Create();
  driver_test().RunInDriverContext([&volume_server](TestNvme& driver) mutable {
    Namespace* ns = driver.namespaces()[0].get();
    ns->ServeRequests(std::move(volume_server));
  });

  zx::result<std::unique_ptr<block_client::RemoteBlockDevice>> client =
      block_client::RemoteBlockDevice::Create(std::move(volume_client));
  ASSERT_OK(client);

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &vmo));
  ::storage::Vmoid vmoid;
  ASSERT_OK(client.value()->BlockAttachVmo(vmo, &vmoid));

  BlockFifoRequest request = {
      .command = {.opcode = BLOCK_OPCODE_READ, .flags = 0},
      .vmoid = vmoid.get(),
      .length = 1,
      .vmo_offset = 0,
      .dev_offset = 0,
  };

  ASSERT_OK(client.value()->FifoTransaction(&request, 1));
  ASSERT_OK(client.value()->BlockDetachVmo(std::move(vmoid)));
  client.value().reset();
}

TEST_F(NvmeTest, DuplicateIoCompletion) {
  fake_nvme::FakeNamespace fake_ns;
  TestNvme::controller_.AddNamespace(1, fake_ns);
  TestNvme::controller_.AddIoCommand(
      IoCommandOpcode::kRead,
      [](Submission& submission, const TransactionData& data, Completion& completion) {
        completion.set_status_code_type(StatusCodeType::kGeneric)
            .set_status_code(GenericStatus::kSuccess);
        // Inject a duplicate completion into the CQ right after this one.
        Completion duplicate_completion = completion;
        TestNvme::controller_.SubmitCompletion(duplicate_completion);
      });
  driver_test().runtime().StartBackgroundDispatcher();

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  auto [volume_client, volume_server] = fidl::Endpoints<fuchsia_storage_block::Block>::Create();
  driver_test().RunInDriverContext([&volume_server](TestNvme& driver) mutable {
    Namespace* ns = driver.namespaces()[0].get();
    ns->ServeRequests(std::move(volume_server));
  });

  zx::result<std::unique_ptr<block_client::RemoteBlockDevice>> client =
      block_client::RemoteBlockDevice::Create(std::move(volume_client));
  ASSERT_OK(client);

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &vmo));
  ::storage::Vmoid vmoid;
  ASSERT_OK(client.value()->BlockAttachVmo(vmo, &vmoid));

  BlockFifoRequest request = {
      .command = {.opcode = BLOCK_OPCODE_READ, .flags = 0},
      .vmoid = vmoid.get(),
      .length = 1,
      .vmo_offset = 0,
      .dev_offset = 0,
  };

  ASSERT_OK(client.value()->FifoTransaction(&request, 1));
  ASSERT_OK(client.value()->BlockDetachVmo(std::move(vmoid)));
  client.value().reset();
}

TEST_F(NvmeTest, NamespaceWriteTest) {
  fake_nvme::FakeNamespace fake_ns;
  TestNvme::controller_.AddNamespace(1, fake_ns);
  TestNvme::controller_.AddIoCommand(
      IoCommandOpcode::kWrite,
      [](Submission& submission, const TransactionData& data, Completion& completion) {
        completion.set_status_code_type(StatusCodeType::kGeneric)
            .set_status_code(GenericStatus::kSuccess);
      });
  driver_test().runtime().StartBackgroundDispatcher();

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  auto [volume_client, volume_server] = fidl::Endpoints<fuchsia_storage_block::Block>::Create();
  driver_test().RunInDriverContext([&volume_server](TestNvme& driver) mutable {
    Namespace* ns = driver.namespaces()[0].get();
    ns->ServeRequests(std::move(volume_server));
  });

  zx::result<std::unique_ptr<block_client::RemoteBlockDevice>> client =
      block_client::RemoteBlockDevice::Create(std::move(volume_client));
  ASSERT_OK(client);

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &vmo));
  ::storage::Vmoid vmoid;
  ASSERT_OK(client.value()->BlockAttachVmo(vmo, &vmoid));

  BlockFifoRequest request = {
      .command = {.opcode = BLOCK_OPCODE_WRITE, .flags = 0},
      .vmoid = vmoid.get(),
      .length = 1,
      .vmo_offset = 0,
      .dev_offset = 0,
  };

  ASSERT_OK(client.value()->FifoTransaction(&request, 1));
  ASSERT_OK(client.value()->BlockDetachVmo(std::move(vmoid)));
  client.value().reset();
}

TEST_F(NvmeTest, InvalidIoRequestFreesCommandAndRepliesWithoutDeadlock) {
  fake_nvme::FakeNamespace fake_ns;
  TestNvme::controller_.AddNamespace(1, fake_ns);
  TestNvme::controller_.AddIoCommand(
      IoCommandOpcode::kRead,
      [](Submission& submission, const TransactionData& data, Completion& completion) {
        completion.set_status_code_type(StatusCodeType::kGeneric)
            .set_status_code(GenericStatus::kSuccess);
      });
  driver_test().runtime().StartBackgroundDispatcher();

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  auto [volume_client, volume_server] = fidl::Endpoints<fuchsia_storage_block::Block>::Create();
  driver_test().RunInDriverContext([&volume_server](TestNvme& driver) mutable {
    Namespace* ns = driver.namespaces()[0].get();
    ns->ServeRequests(std::move(volume_server));
  });

  zx::result<std::unique_ptr<block_client::RemoteBlockDevice>> client =
      block_client::RemoteBlockDevice::Create(std::move(volume_client));
  ASSERT_OK(client);

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &vmo));
  ::storage::Vmoid vmoid;
  ASSERT_OK(client.value()->BlockAttachVmo(vmo, &vmoid));

  // Issue more than kMaxRequests (64) out-of-range reads to verify Namespace::OnRequests error
  // paths neither deadlock on Namespace::lock_ nor leak IoCommand slots.
  for (size_t i = 0; i < 70; ++i) {
    BlockFifoRequest out_of_range = {
        .command = {.opcode = BLOCK_OPCODE_READ, .flags = 0},
        .vmoid = vmoid.get(),
        .length = 1,
        .vmo_offset = 0,
        .dev_offset = 1024,
    };
    EXPECT_STATUS(client.value()->FifoTransaction(&out_of_range, 1), ZX_ERR_OUT_OF_RANGE);
  }

  BlockFifoRequest valid_read = {
      .command = {.opcode = BLOCK_OPCODE_READ, .flags = 0},
      .vmoid = vmoid.get(),
      .length = 1,
      .vmo_offset = 0,
      .dev_offset = 0,
  };
  EXPECT_OK(client.value()->FifoTransaction(&valid_read, 1));
  ASSERT_OK(client.value()->BlockDetachVmo(std::move(vmoid)));
  client.value().reset();
}

TEST_F(NvmeTeardownTest, TeardownWithActiveClient) {
  fake_nvme::FakeNamespace fake_ns;
  TestNvme::controller_.AddNamespace(1, fake_ns);
  TestNvme::controller_.AddIoCommand(
      IoCommandOpcode::kRead,
      [](Submission& submission, const TransactionData& data, Completion& completion) {
        completion.set_status_code_type(StatusCodeType::kGeneric)
            .set_status_code(GenericStatus::kSuccess);
      });
  driver_test().runtime().StartBackgroundDispatcher();

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  auto [volume_client, volume_server] = fidl::Endpoints<fuchsia_storage_block::Block>::Create();
  driver_test().RunInDriverContext([&volume_server](TestNvme& driver) mutable {
    Namespace* ns = driver.namespaces()[0].get();
    ns->ServeRequests(std::move(volume_server));
  });

  zx::result<std::unique_ptr<block_client::RemoteBlockDevice>> device =
      block_client::RemoteBlockDevice::Create(std::move(volume_client));
  ASSERT_OK(device);

  block_client::ReaderWriter client(*device.value());

  std::atomic<bool> stopped = false;
  std::thread t([&]() {
    while (!stopped) {
      uint8_t buffer[512];
      [[maybe_unused]] zx_status_t status = client.Read(0, sizeof(buffer), buffer);
    }
  });

  EXPECT_OK(driver_test().StopDriver());
  stopped = true;
  t.join();
}

TEST_F(NvmeTeardownTest, TeardownWithPendingRequest) {
  fake_nvme::FakeNamespace fake_ns;
  TestNvme::controller_.AddNamespace(1, fake_ns);

  libsync::Completion unblock;
  libsync::Completion in_flight;
  // Start a command which blocks, so the driver cannot issue any other commands.
  TestNvme::controller_.AddIoCommand(
      IoCommandOpcode::kRead,
      [&](Submission& submission, const TransactionData& data, Completion& completion) {
        in_flight.Signal();
        unblock.Wait();
        completion.set_status_code_type(StatusCodeType::kGeneric)
            .set_status_code(GenericStatus::kSuccess);
      });
  driver_test().runtime().StartBackgroundDispatcher();

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  auto [volume_client, volume_server] = fidl::Endpoints<fuchsia_storage_block::Block>::Create();
  driver_test().RunInDriverContext([&volume_server](TestNvme& driver) mutable {
    Namespace* ns = driver.namespaces()[0].get();
    ns->ServeRequests(std::move(volume_server));
  });

  zx::result<std::unique_ptr<block_client::RemoteBlockDevice>> device =
      block_client::RemoteBlockDevice::Create(std::move(volume_client));
  ASSERT_OK(device);

  block_client::ReaderWriter client(*device.value());

  std::thread t([&]() {
    uint8_t buffer[512];
    [[maybe_unused]] zx_status_t status = client.Read(0, sizeof(buffer), buffer);
  });

  // Wait for the blocking command to reach the hardware.
  in_flight.Wait();

  std::thread unblock_thread([&]() {
    // Give StopDriver() time to reach ns->StopBlockServer() and block_server_->DestroyAsync().
    zx::nanosleep(zx::deadline_after(zx::msec(50)));
    unblock.Signal();
  });

  EXPECT_OK(driver_test().StopDriver());

  unblock_thread.join();
  t.join();
}

TEST_F(NvmeTest, IoPushback) {
  fake_nvme::FakeNamespace fake_ns;
  TestNvme::controller_.AddNamespace(1, fake_ns);

  // Block I/O so we can queue up a bunch of requests.
  libsync::Completion unblock;
  TestNvme::controller_.AddIoCommand(
      IoCommandOpcode::kRead,
      [&](Submission&, const TransactionData&, Completion&) { unblock.Wait(); });

  driver_test().runtime().StartBackgroundDispatcher();
  ASSERT_NO_FATAL_FAILURE(StartDriver());

  auto [volume_client, volume_server] = fidl::Endpoints<fuchsia_storage_block::Block>::Create();
  driver_test().RunInDriverContext([&volume_server](TestNvme& driver) mutable {
    Namespace* ns = driver.namespaces()[0].get();
    ns->ServeRequests(std::move(volume_server));
  });

  auto [session_client, session_server] = fidl::Endpoints<fuchsia_storage_block::Session>::Create();
  fidl::OneWayStatus result = fidl::WireCall(volume_client)->OpenSession(std::move(session_server));
  ASSERT_OK(result.status());

  fidl::WireResult fifo = fidl::WireCall(session_client)->GetFifo();
  ASSERT_OK(fifo.status());

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &vmo));
  fidl::WireResult attach_result = fidl::WireCall(session_client)->AttachVmo(std::move(vmo));
  ASSERT_OK(attach_result.status());
  vmoid_t vmoid = attach_result.value()->vmoid.id;
  [[maybe_unused]] auto cleanup = fit::defer([&] {
    BlockFifoRequest request = {
        .command = {.opcode = BLOCK_OPCODE_CLOSE_VMO, .flags = 0},
        .vmoid = vmoid,
    };
    size_t actual_count = 0;
    ASSERT_OK(fifo->value()->fifo.write(sizeof(request), &request, 1, &actual_count));
    ASSERT_EQ(actual_count, 1u);
    ASSERT_OK(fifo->value()->fifo.wait_one(ZX_FIFO_READABLE, zx::time::infinite(), nullptr));
    BlockFifoResponse response;
    ASSERT_OK(fifo->value()->fifo.read(sizeof(response), &response, 1, &actual_count));
    ASSERT_EQ(actual_count, 1u);
    ASSERT_OK(response.status);
  });

  // Eventually the client should get pushback via ZX_ERR_SHOULD_WAIT.
  size_t num_requests = 0;
  while (true) {
    BlockFifoRequest request = {
        .command = {.opcode = BLOCK_OPCODE_READ, .flags = 0},
        .vmoid = vmoid,
        .length = 1,
        .vmo_offset = 0,
        .dev_offset = 0,
    };
    size_t actual_count = 0;
    zx_status_t status = fifo->value()->fifo.write(sizeof(request), &request, 1, &actual_count);
    if (status == ZX_ERR_SHOULD_WAIT) {
      break;
    }
    ASSERT_OK(status);
    ASSERT_EQ(actual_count, 1u);
    ++num_requests;
  }
  ASSERT_GT(num_requests, 0u);

  // Unblock I/O, and then we should eventually be able to read one response and write another
  // request to the FIFO.
  unblock.Signal();

  BlockFifoResponse response;
  ASSERT_OK(fifo.value()->fifo.wait_one(ZX_FIFO_READABLE, zx::time::infinite(), nullptr));
  size_t actual_count = 0;
  ASSERT_OK(fifo->value()->fifo.read(sizeof(response), &response, 1, &actual_count));
  ASSERT_EQ(actual_count, 1u);
  ASSERT_OK(response.status);
  --num_requests;

  BlockFifoRequest request = {
      .command = {.opcode = BLOCK_OPCODE_READ, .flags = 0},
      .vmoid = vmoid,
      .length = 1,
      .vmo_offset = 0,
      .dev_offset = 0,
  };
  ASSERT_OK(fifo->value()->fifo.wait_one(ZX_FIFO_WRITABLE, zx::time::infinite(), nullptr));
  ASSERT_OK(fifo->value()->fifo.write(sizeof(request), &request, 1, &actual_count));
  ASSERT_EQ(actual_count, 1u);

  // Drain the rest of the requests and ensure that nothing failed.
  while (num_requests > 0) {
    ASSERT_OK(fifo->value()->fifo.wait_one(ZX_FIFO_READABLE, zx::time::infinite(), nullptr));
    ASSERT_OK(fifo->value()->fifo.read(sizeof(response), &response, 1, &actual_count));
    ASSERT_EQ(actual_count, 1u);
    ASSERT_OK(response.status);
    --num_requests;
  }

  ASSERT_OK(fifo->value()->fifo.wait_one(ZX_FIFO_READABLE, zx::time::infinite(), nullptr));
  ASSERT_OK(fifo->value()->fifo.read(sizeof(response), &response, 1, &actual_count));
  ASSERT_EQ(actual_count, 1u);
  ASSERT_OK(response.status);
}

TEST_F(NvmeTest, NodeToken) {
  fake_nvme::FakeNamespace fake_ns;
  TestNvme::controller_.AddNamespace(1, fake_ns);
  driver_test().runtime().StartBackgroundDispatcher();

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  zx::result connect_result =
      driver_test().Connect<fuchsia_hardware_block_volume::Service::Token>("namespace-1");
  ASSERT_OK(connect_result);

  fidl::SyncClient<fuchsia_driver_token::NodeToken> client(std::move(connect_result.value()));
  auto get_result = client->Get();
  ASSERT_TRUE(get_result.is_ok());

  zx_info_handle_basic_t info1, info2;
  ASSERT_EQ(node_token_.get_info(ZX_INFO_HANDLE_BASIC, &info1, sizeof(info1), nullptr, nullptr),
            ZX_OK);
  ASSERT_EQ(
      get_result->token().get_info(ZX_INFO_HANDLE_BASIC, &info2, sizeof(info2), nullptr, nullptr),
      ZX_OK);
  ASSERT_EQ(info1.koid, info2.koid);
}

TEST_F(NvmeTest, InitFailureCleansUpThreads) {
  TestNvme::controller_.AddAdminCommand(
      nvme::AdminCommandOpcode::kIdentify,
      [](Submission& default_submission, const TransactionData& data, Completion& completion) {
        completion.set_status_code_type(StatusCodeType::kGeneric)
            .set_status_code(GenericStatus::kInvalidField);
      });

  zx::result<> result = StartDriverResult();
  EXPECT_TRUE(result.is_error());
  EXPECT_FALSE(TestNvme::controller_.bus_master_enabled());
}

TEST_F(NvmeTest, TwoVectorMsixUsesDedicatedIoVectorAndAvoidsAdminAddrReads) {
  fake_nvme::FakeNamespace fake_ns;
  TestNvme::controller_.AddNamespace(1, fake_ns);
  TestNvme::controller_.AddIoCommand(
      IoCommandOpcode::kRead,
      [](Submission& submission, const TransactionData& data, Completion& completion) {
        completion.set_status_code_type(StatusCodeType::kGeneric)
            .set_status_code(GenericStatus::kSuccess);
      });
  driver_test().runtime().StartBackgroundDispatcher();

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  EXPECT_EQ(2u, TestNvme::controller_.configured_irq_count());
  EXPECT_EQ(1u, TestNvme::controller_.mapped_irq_vector());
  EXPECT_EQ(1u, TestNvme::controller_.io_cq_interrupt_vector());
  EXPECT_EQ(0u, TestNvme::controller_.registers().admin_queue_addr_reads());

  auto [volume_client, volume_server] = fidl::Endpoints<fuchsia_storage_block::Block>::Create();
  driver_test().RunInDriverContext([&volume_server](TestNvme& driver) mutable {
    EXPECT_EQ(1u, driver.io_irq_vector());
    EXPECT_EQ(fuchsia_hardware_pci::InterruptMode::kMsiX, driver.irq_mode());
    driver.namespaces()[0]->ServeRequests(std::move(volume_server));
  });

  zx::result<std::unique_ptr<block_client::RemoteBlockDevice>> client =
      block_client::RemoteBlockDevice::Create(std::move(volume_client));
  ASSERT_OK(client);

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &vmo));
  ::storage::Vmoid vmoid;
  ASSERT_OK(client.value()->BlockAttachVmo(vmo, &vmoid));

  BlockFifoRequest request = {
      .command = {.opcode = BLOCK_OPCODE_READ, .flags = 0},
      .vmoid = vmoid.get(),
      .length = 1,
      .vmo_offset = 0,
      .dev_offset = 0,
  };
  ASSERT_OK(client.value()->FifoTransaction(&request, 1));
  ASSERT_OK(client.value()->BlockDetachVmo(std::move(vmoid)));
  client.value().reset();

  inspect::Inspector inspector = driver_test().RunInDriverContext<inspect::Inspector>(
      [](TestNvme& driver) { return driver.inspect(); });
  fpromise::result<inspect::Hierarchy> hierarchy =
      fpromise::run_single_threaded(inspect::ReadFromInspector(inspector));
  ASSERT_TRUE(hierarchy.is_ok());
  const auto* nvme = hierarchy.value().GetByPath({"nvme"});
  ASSERT_NE(nullptr, nvme);
  CheckUintProperty(nvme->node(), "io_irq_vector", 1);
  const auto* irq_count = nvme->node().get_property<inspect::UintPropertyValue>("interrupt_count");
  ASSERT_NE(nullptr, irq_count);
  EXPECT_GE(irq_count->value(), 1u);
}

TEST_F(NvmeTest, SingleVectorMsixFallbackUsesVectorZero) {
  TestNvme::controller_.SetMaxIrqs(1);
  fake_nvme::FakeNamespace fake_ns;
  TestNvme::controller_.AddNamespace(1, fake_ns);
  TestNvme::controller_.AddIoCommand(
      IoCommandOpcode::kRead,
      [](Submission& submission, const TransactionData& data, Completion& completion) {
        completion.set_status_code_type(StatusCodeType::kGeneric)
            .set_status_code(GenericStatus::kSuccess);
      });
  driver_test().runtime().StartBackgroundDispatcher();

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  EXPECT_EQ(1u, TestNvme::controller_.configured_irq_count());
  EXPECT_EQ(0u, TestNvme::controller_.mapped_irq_vector());
  EXPECT_EQ(0u, TestNvme::controller_.io_cq_interrupt_vector());

  auto [volume_client, volume_server] = fidl::Endpoints<fuchsia_storage_block::Block>::Create();
  driver_test().RunInDriverContext([&volume_server](TestNvme& driver) mutable {
    EXPECT_EQ(0u, driver.io_irq_vector());
    driver.namespaces()[0]->ServeRequests(std::move(volume_server));
  });

  zx::result<std::unique_ptr<block_client::RemoteBlockDevice>> client =
      block_client::RemoteBlockDevice::Create(std::move(volume_client));
  ASSERT_OK(client);

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &vmo));
  ::storage::Vmoid vmoid;
  ASSERT_OK(client.value()->BlockAttachVmo(vmo, &vmoid));

  BlockFifoRequest request = {
      .command = {.opcode = BLOCK_OPCODE_READ, .flags = 0},
      .vmoid = vmoid.get(),
      .length = 1,
      .vmo_offset = 0,
      .dev_offset = 0,
  };
  ASSERT_OK(client.value()->FifoTransaction(&request, 1));
  ASSERT_OK(client.value()->BlockDetachVmo(std::move(vmoid)));
  client.value().reset();
}

TEST_F(NvmeTest, MultiVectorMsiFallsBackToSingleSharedVectorZero) {
  // A device without 2 MSI-X vectors that exposes multiple MSI vectors must fall back to 1 shared
  // vector 0 rather than configuring 2 MSI vectors with vector 0 unmapped.
  TestNvme::controller_.SetMaxIrqs(2);
  TestNvme::controller_.SetSupportedIrqMode(fuchsia_hardware_pci::InterruptMode::kMsi);
  fake_nvme::FakeNamespace fake_ns;
  TestNvme::controller_.AddNamespace(1, fake_ns);
  TestNvme::controller_.AddIoCommand(
      IoCommandOpcode::kRead,
      [](Submission& submission, const TransactionData& data, Completion& completion) {
        completion.set_status_code_type(StatusCodeType::kGeneric)
            .set_status_code(GenericStatus::kSuccess);
      });
  driver_test().runtime().StartBackgroundDispatcher();

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  EXPECT_EQ(1u, TestNvme::controller_.configured_irq_count());
  EXPECT_EQ(0u, TestNvme::controller_.mapped_irq_vector());
  EXPECT_EQ(0u, TestNvme::controller_.io_cq_interrupt_vector());

  auto [volume_client, volume_server] = fidl::Endpoints<fuchsia_storage_block::Block>::Create();
  driver_test().RunInDriverContext([&volume_server](TestNvme& driver) mutable {
    EXPECT_EQ(fuchsia_hardware_pci::InterruptMode::kMsi, driver.irq_mode());
    EXPECT_EQ(0u, driver.io_irq_vector());
    driver.namespaces()[0]->ServeRequests(std::move(volume_server));
  });

  zx::result<std::unique_ptr<block_client::RemoteBlockDevice>> client =
      block_client::RemoteBlockDevice::Create(std::move(volume_client));
  ASSERT_OK(client);

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &vmo));
  ::storage::Vmoid vmoid;
  ASSERT_OK(client.value()->BlockAttachVmo(vmo, &vmoid));

  BlockFifoRequest request = {
      .command = {.opcode = BLOCK_OPCODE_READ, .flags = 0},
      .vmoid = vmoid.get(),
      .length = 1,
      .vmo_offset = 0,
      .dev_offset = 0,
  };
  ASSERT_OK(client.value()->FifoTransaction(&request, 1));
  ASSERT_OK(client.value()->BlockDetachVmo(std::move(vmoid)));
  client.value().reset();
}

TEST_F(NvmeTest, InitResetsActiveControllerBeforeEnablingBusMasteringAndInterrupts) {
  // Simulate UEFI/firmware leaving the controller enabled, ready, and faulted (CSTS.CFS=1) at
  // driver bind time; WaitForReset(false) must still let CC.EN=0 clear RDY and CFS cleanly.
  TestNvme::controller_.registers().ccfg().set_enabled(1);
  TestNvme::controller_.registers().csts().set_ready(true);
  TestNvme::controller_.registers().csts().set_controller_fatal_status(true);

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  EXPECT_FALSE(TestNvme::controller_.irq_configured_while_controller_ready());
  EXPECT_FALSE(TestNvme::controller_.irq_mapped_while_controller_ready());
  EXPECT_FALSE(TestNvme::controller_.bme_enabled_while_controller_ready());
  EXPECT_TRUE(TestNvme::controller_.bus_master_enabled());
  EXPECT_FALSE(TestNvme::controller_.registers().csts().controller_fatal_status());
}

TEST_F(NvmeTest, NonMsixInterruptOrdersMaskSetBeforeDoorbellBeforeMaskClear) {
  TestNvme::controller_.SetMaxIrqs(2);
  TestNvme::controller_.SetSupportedIrqMode(fuchsia_hardware_pci::InterruptMode::kMsi);
  fake_nvme::FakeNamespace fake_ns;
  TestNvme::controller_.AddNamespace(1, fake_ns);
  TestNvme::controller_.AddIoCommand(
      IoCommandOpcode::kRead,
      [](Submission& submission, const TransactionData& data, Completion& completion) {
        TestNvme::controller_.registers().ClearIrqOrderingEvents();
        completion.set_status_code_type(StatusCodeType::kGeneric)
            .set_status_code(GenericStatus::kSuccess);
      });
  driver_test().runtime().StartBackgroundDispatcher();

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  auto [volume_client, volume_server] = fidl::Endpoints<fuchsia_storage_block::Block>::Create();
  driver_test().RunInDriverContext([&volume_server](TestNvme& driver) mutable {
    EXPECT_EQ(0u, driver.io_irq_vector());
    driver.namespaces()[0]->ServeRequests(std::move(volume_server));
  });

  zx::result<std::unique_ptr<block_client::RemoteBlockDevice>> client =
      block_client::RemoteBlockDevice::Create(std::move(volume_client));
  ASSERT_OK(client);

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &vmo));
  ::storage::Vmoid vmoid;
  ASSERT_OK(client.value()->BlockAttachVmo(vmo, &vmoid));

  BlockFifoRequest request = {
      .command = {.opcode = BLOCK_OPCODE_READ, .flags = 0},
      .vmoid = vmoid.get(),
      .length = 1,
      .vmo_offset = 0,
      .dev_offset = 0,
  };
  ASSERT_OK(client.value()->FifoTransaction(&request, 1));
  ASSERT_OK(client.value()->BlockDetachVmo(std::move(vmoid)));
  client.value().reset();

  // RingCompletionDb() and INTMC write run immediately after io_cmd->Complete(ZX_OK) on
  // nvme-io-thread; wait briefly for all 3 MMIO events to be recorded.
  const zx::time deadline = zx::deadline_after(zx::sec(2));
  while (TestNvme::controller_.registers().irq_ordering_events().size() < 3 &&
         zx::clock::get_monotonic() < deadline) {
    zx::nanosleep(zx::deadline_after(zx::msec(1)));
  }

  using Type = fake_nvme::FakeRegisters::IrqOrderingEventType;
  const auto events = TestNvme::controller_.registers().irq_ordering_events();
  ASSERT_EQ(3u, events.size());
  EXPECT_EQ(Type::kMaskSet, events[0].type);
  EXPECT_EQ(1u << 0, events[0].value);
  EXPECT_EQ(Type::kIoCqDoorbell, events[1].type);
  EXPECT_EQ(Type::kMaskClear, events[2].type);
  EXPECT_EQ(1u << 0, events[2].value);
}

TEST_F(NvmeTest, MaxDataTransferLargeShiftClampsWithoutOverflowOrUb) {
  // MDTS = 31 would invoke signed shift UB (`1 << 31`) and overflow uint32_t if unguarded.
  TestNvme::controller_.SetMaxDataTransfer(31);

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  driver_test().RunInDriverContext([](TestNvme& driver) {
    EXPECT_EQ(std::numeric_limits<uint32_t>::max(), driver.max_data_transfer_bytes());
  });
}

TEST_F(NvmeTest, UnsupportedNvmCommandSetFailsInit) {
  TestNvme::controller_.registers().caps().set_nvm_command_set_support(false);
  zx::result<> result = StartDriverResult();
  EXPECT_STATUS(result.status_value(), ZX_ERR_NOT_SUPPORTED);
}

TEST_F(NvmeTest, ControllerFatalStatusFailsResetImmediately) {
  TestNvme::controller_.registers().csts().set_controller_fatal_status(true);
  zx::result<> result = StartDriverResult();
  EXPECT_STATUS(result.status_value(), ZX_ERR_IO);
  // ConfigureInterrupt() enabled MSI-X bus mastering in the PCI bus driver before CC.EN=1 failed;
  // PerformTeardown() must still disable PCI bus mastering.
  EXPECT_FALSE(TestNvme::controller_.bus_master_enabled());
}

TEST_F(NvmeTest, AdminCommandMatchesCidAndSkipsInvalidCompletions) {
  ASSERT_NO_FATAL_FAILURE(StartDriver());

  // 1. Inject a CQE for an inactive CID ahead of the real CQE and verify DoAdminCommandSync skips
  // the ZX_ERR_BAD_STATE completion.
  TestNvme::controller_.AddAsyncAdminCommand(
      nvme::AdminCommandOpcode::kGetFeatures, [](Submission& submission, const TransactionData&) {
        Completion stale{};
        stale.set_command_id(static_cast<uint16_t>((submission.cid() + 5) % 16))
            .set_sq_id(0)
            .set_sq_head(0)
            .set_status_code_type(StatusCodeType::kGeneric)
            .set_status_code(GenericStatus::kInvalidField);
        TestNvme::controller_.SubmitCompletion(stale);

        Completion valid{};
        valid.set_command_id(static_cast<uint16_t>(submission.cid())).set_sq_id(0).set_sq_head(0);
        auto& vwc = valid.GetCompletion<GetVolatileWriteCacheCompletion>();
        vwc.set_volatile_write_cache_enabled(true);
        vwc.set_status_code_type(StatusCodeType::kGeneric);
        vwc.set_status_code(GenericStatus::kSuccess);
        TestNvme::controller_.SubmitCompletion(valid);
      });

  driver_test().RunInDriverContext([](TestNvme& driver) {
    GetVolatileWriteCacheSubmission cmd;
    zx::result<Completion> res = driver.DoAdminCommandSync(cmd);
    ASSERT_OK(res);
    EXPECT_TRUE(
        res->GetCompletion<GetVolatileWriteCacheCompletion>().get_volatile_write_cache_enabled());
  });

  // 2. Leave an active admin submission in flight, then issue DoAdminCommandSync() whose handler
  // posts a CQE for that other active CID ahead of DoAdminCommandSync()'s own CQE, exercising the
  // status == ZX_OK && cqe.command_id() != cid path and verifying zero slots remain in flight.
  TestNvme::controller_.AddAsyncAdminCommand(nvme::AdminCommandOpcode::kGetFeatures,
                                             [](Submission&, const TransactionData&) {});
  GetVolatileWriteCacheSubmission dummy_cmd;
  zx::result<uint16_t> dummy_cid = TestNvme::controller_.SubmitDummyAdminSubmission(dummy_cmd);
  ASSERT_OK(dummy_cid);
  EXPECT_EQ(1u, TestNvme::controller_.admin_in_flight_count());

  TestNvme::controller_.AddAsyncAdminCommand(
      nvme::AdminCommandOpcode::kGetFeatures, [&](Submission& submission, const TransactionData&) {
        Completion other{};
        other.set_command_id(dummy_cid.value())
            .set_sq_id(0)
            .set_sq_head(0)
            .set_status_code_type(StatusCodeType::kGeneric)
            .set_status_code(GenericStatus::kInvalidField);
        TestNvme::controller_.SubmitCompletion(other);

        Completion valid{};
        valid.set_command_id(static_cast<uint16_t>(submission.cid())).set_sq_id(0).set_sq_head(0);
        auto& vwc = valid.GetCompletion<GetVolatileWriteCacheCompletion>();
        vwc.set_volatile_write_cache_enabled(false);
        vwc.set_status_code_type(StatusCodeType::kGeneric);
        vwc.set_status_code(GenericStatus::kSuccess);
        TestNvme::controller_.SubmitCompletion(valid);
      });

  driver_test().RunInDriverContext([](TestNvme& driver) {
    GetVolatileWriteCacheSubmission cmd;
    zx::result<Completion> res = driver.DoAdminCommandSync(cmd);
    ASSERT_OK(res);
    EXPECT_FALSE(
        res->GetCompletion<GetVolatileWriteCacheCompletion>().get_volatile_write_cache_enabled());
    EXPECT_EQ(0u, TestNvme::controller_.admin_in_flight_count());
  });
}

TEST_F(NvmeTeardownTest, TeardownPerformsNormalShutdownAndCancelsSubmittedInFlightIo) {
  fake_nvme::FakeNamespace fake_ns;
  TestNvme::controller_.AddNamespace(1, fake_ns);

  libsync::Completion in_flight;
  // Accept the Read into hardware SQ 1 without ever posting a CQE (simulating a wedged controller).
  TestNvme::controller_.AddAsyncIoCommand(
      IoCommandOpcode::kRead, [&](Submission&, const TransactionData&) { in_flight.Signal(); });
  driver_test().runtime().StartBackgroundDispatcher();

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  auto [volume_client, volume_server] = fidl::Endpoints<fuchsia_storage_block::Block>::Create();
  driver_test().RunInDriverContext([&volume_server](TestNvme& driver) mutable {
    driver.namespaces()[0]->ServeRequests(std::move(volume_server));
  });

  zx::result<std::unique_ptr<block_client::RemoteBlockDevice>> device =
      block_client::RemoteBlockDevice::Create(std::move(volume_client));
  ASSERT_OK(device);

  block_client::ReaderWriter client(*device.value());
  zx_status_t read_status = ZX_OK;
  std::thread t([&]() {
    uint8_t buffer[512];
    read_status = client.Read(0, sizeof(buffer), buffer);
  });

  in_flight.Wait();

  size_t pinned_before = 0;
  ASSERT_OK(fake_bti_get_pinned_vmos(TestNvme::last_bti_.get(), nullptr, 0, &pinned_before));
  EXPECT_GE(pinned_before, 1u);

  EXPECT_OK(driver_test().StopDriver());
  t.join();

  // Verify DoAdminCommandSync() rejects calls after PerformTeardown() sets driver_shutdown_.
  driver_test().RunInDriverContext([](TestNvme& driver) {
    GetVolatileWriteCacheSubmission cmd;
    EXPECT_STATUS(driver.DoAdminCommandSync(cmd).status_value(), ZX_ERR_CANCELED);
  });

  size_t pinned_after = 0;
  ASSERT_OK(fake_bti_get_pinned_vmos(TestNvme::last_bti_.get(), nullptr, 0, &pinned_after));
  EXPECT_EQ(pinned_before - 1, pinned_after);
  EXPECT_STATUS(read_status, ZX_ERR_PEER_CLOSED);
  EXPECT_TRUE(TestNvme::controller_.saw_normal_shutdown());
  EXPECT_FALSE(TestNvme::controller_.registers().ccfg().enabled());
  EXPECT_FALSE(TestNvme::controller_.registers().csts().ready());
  EXPECT_TRUE(TestNvme::controller_.bme_disabled_after_normal_shutdown());
  EXPECT_TRUE(TestNvme::controller_.bme_disabled_after_controller_disabled());
  EXPECT_FALSE(TestNvme::controller_.bus_master_enabled());
}

}  // namespace nvme

FUCHSIA_DRIVER_EXPORT2(nvme::TestNvme);
