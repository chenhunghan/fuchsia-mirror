// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/nand/drivers/nandpart/nandpart.h"

#include <fidl/fuchsia.boot.metadata/cpp/fidl.h>
#include <fidl/fuchsia.driver.compat/cpp/markers.h>
#include <fidl/fuchsia.driver.compat/cpp/wire_messaging.h>
#include <fidl/fuchsia.hardware.badblock/cpp/wire.h>
#include <fidl/fuchsia.hardware.nand/cpp/common_types.h>
#include <fidl/fuchsia.hardware.nand/cpp/natural_types.h>
#include <fuchsia/hardware/nand/c/banjo.h>
#include <fuchsia/hardware/nand/cpp/banjo.h>
#include <fuchsia/hardware/nandinfo/c/banjo.h>
#include <lib/async/dispatcher.h>
#include <lib/ddk/driver.h>
#include <lib/ddk/metadata.h>
#include <lib/driver/compat/cpp/banjo_client.h>
#include <lib/driver/compat/cpp/banjo_server.h>
#include <lib/driver/compat/cpp/device_server.h>
#include <lib/driver/metadata/cpp/metadata_server.h>
#include <lib/driver/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/fidl/cpp/natural_types.h>
#include <lib/fidl/cpp/wire/client.h>
#include <lib/fit/internal/result.h>
#include <lib/sync/completion.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>
#include <zircon/errors.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <cstddef>
#include <cstdint>
#include <optional>
#include <set>
#include <string_view>
#include <utility>
#include <vector>

#include <gtest/gtest.h>

#include "src/lib/testing/predicates/status.h"

namespace nand::testing {

class FakeNand : public ddk::NandProtocol<FakeNand> {
 public:
  void set_nand_info(const nand_info_t& info) { info_ = info; }

  compat::DeviceServer::BanjoConfig GetBanjoConfig() {
    compat::DeviceServer::BanjoConfig config{ZX_PROTOCOL_NAND};
    config.callbacks[ZX_PROTOCOL_NAND] = banjo_server_.callback();
    return config;
  }

  // Nand protocol implementation.
  void NandQuery(nand_info_t* info_out, size_t* nand_op_size_out) {
    *info_out = info_;
    *nand_op_size_out = sizeof(nand_operation_t);
  }

  void NandQueue(nand_operation_t* op, nand_queue_callback completion_cb, void* cookie) {
    const size_t data_bytes = static_cast<size_t>(op->rw.length) * info_.page_size;
    const uint64_t data_offset_bytes = op->rw.offset_data_vmo * info_.page_size;
    const size_t oob_bytes = static_cast<size_t>(op->rw.length) * info_.oob_size;
    const uint64_t oob_offset_bytes = op->rw.offset_oob_vmo * info_.page_size;

    if (op->command == NAND_OP_READ && zx::unowned_vmo(op->rw.data_vmo)->is_valid()) {
      std::vector<uint8_t> data(data_bytes, 0);
      for (uint32_t b : bad_blocks_) {
        if (b < data.size()) {
          data[b] = 1;
        }
      }
      zx_status_t status =
          zx::unowned_vmo(op->rw.data_vmo)->write(data.data(), data_offset_bytes, data.size());
      if (status != ZX_OK) {
        completion_cb(cookie, status, op);
        return;
      }

      if (zx::unowned_vmo(op->rw.oob_vmo)->is_valid() && info_.oob_size > 0) {
        struct Oob {
          uint32_t magic;
          uint16_t generation;
          uint16_t ec;
        };
        if (oob_bytes < sizeof(Oob)) {
          completion_cb(cookie, ZX_ERR_BUFFER_TOO_SMALL, op);
          return;
        }
        Oob oob{
            .magic = 0x7462626E,
            .generation = generation_,
            .ec = 0,
        };
        status = zx::unowned_vmo(op->rw.oob_vmo)->write(&oob, oob_offset_bytes, sizeof(oob));
        if (status != ZX_OK) {
          completion_cb(cookie, status, op);
          return;
        }
      }
      completion_cb(cookie, ZX_OK, op);
      return;
    }

    if (op->command == NAND_OP_WRITE && zx::unowned_vmo(op->rw.data_vmo)->is_valid()) {
      std::vector<uint8_t> data(data_bytes, 0);
      zx_status_t status =
          zx::unowned_vmo(op->rw.data_vmo)->read(data.data(), data_offset_bytes, data.size());
      if (status != ZX_OK) {
        completion_cb(cookie, status, op);
        return;
      }
      bad_blocks_.clear();
      for (size_t b = 0; b < data.size(); ++b) {
        if (data[b] != 0) {
          bad_blocks_.insert(static_cast<uint32_t>(b));
        }
      }
      generation_++;
      completion_cb(cookie, ZX_OK, op);
      return;
    }

    completion_cb(cookie, ZX_OK, op);
  }

  zx_status_t NandGetFactoryBadBlockList(uint32_t* bad_blocks, size_t bad_block_len,
                                         size_t* num_bad_blocks) {
    return ZX_ERR_NOT_SUPPORTED;
  }

 private:
  compat::BanjoServer banjo_server_{ZX_PROTOCOL_NAND, this, &nand_protocol_ops_};
  nand_info_t info_ = {
      .page_size = 1,
      .pages_per_block = 1,
      .num_blocks = 100,
      .ecc_bits = 0,
      .oob_size = 0,
  };
  std::set<uint32_t> bad_blocks_;
  uint16_t generation_ = 1;
};

class NandpartTestEnvironment : public fdf_testing::Environment {
 public:
  void Init(const fuchsia_hardware_nand::Config& nand_config,
            fuchsia_boot_metadata::PartitionMap partition_map,
            std::optional<nand_info_t> nand_info = std::nullopt) {
    if (nand_info) {
      nand_.set_nand_info(*nand_info);
    }
    device_server_.Initialize("default", std::nullopt, nand_.GetBanjoConfig());

    {
      fit::result persisted = fidl::Persist(nand_config);
      ASSERT_TRUE(persisted.is_ok());
      device_server_.AddMetadata(DEVICE_METADATA_PRIVATE, persisted.value().data(),
                                 persisted.value().size());
    }

    partition_map_ = std::move(partition_map);
  }

  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();

    EXPECT_OK(device_server_.Serve(dispatcher, &to_driver_vfs));
    EXPECT_OK(partition_map_metadata_server_.Serve(to_driver_vfs, dispatcher, partition_map_));

    return zx::ok();
  }

 private:
  FakeNand nand_;
  compat::DeviceServer device_server_;
  fdf_metadata::MetadataServer<fuchsia_boot_metadata::PartitionMap> partition_map_metadata_server_;
  fuchsia_boot_metadata::PartitionMap partition_map_;
};

class FixtureConfig final {
 public:
  using DriverType = Driver;
  using EnvironmentType = NandpartTestEnvironment;
};

class NandpartTest : public ::testing::Test {
 public:
  void TearDown() override { ASSERT_OK(driver_test_.StopDriver()); }

 protected:
  void StartDriver(const fuchsia_hardware_nand::Config& nand_config,
                   const fuchsia_boot_metadata::PartitionMap& partition_map,
                   std::optional<nand_info_t> nand_info = std::nullopt) {
    driver_test_.RunInEnvironmentTypeContext(
        [&](NandpartTestEnvironment& env) { env.Init(nand_config, partition_map, nand_info); });
    ASSERT_OK(driver_test_.StartDriver());
  }

  template <typename BanjoClient>
  BanjoClient ConnectToBanjo(std::string_view partition_name) {
    static const uint64_t kProcessKoid = compat::internal::GetKoid();

    zx::result compat_client_end =
        driver_test_.Connect<fuchsia_driver_compat::Service::Device>(partition_name);
    EXPECT_OK(compat_client_end);
    fidl::WireClient<fuchsia_driver_compat::Device> compat(
        std::move(compat_client_end.value()),
        driver_test_.runtime().GetForegroundDispatcher()->async_dispatcher());

    zx::result<BanjoClient> banjo_client;
    compat->GetBanjoProtocol(BanjoClient::kProtocolId, kProcessKoid)
        .ThenExactlyOnce(
            [&](fidl::WireUnownedResult<fuchsia_driver_compat::Device::GetBanjoProtocol>& result) {
              ASSERT_OK(result.status());
              banjo_client = compat::internal::OnResult<BanjoClient>(result);
              driver_test_.runtime().Quit();
            });

    driver_test_.runtime().Run();
    driver_test_.runtime().ResetQuit();
    EXPECT_OK(banjo_client);
    EXPECT_TRUE(banjo_client.value().is_valid());
    return banjo_client.value();
  }

  fidl::WireClient<fuchsia_hardware_badblock::BadBlock> ConnectToBadBlock(
      std::string_view partition_name) {
    zx::result client_end =
        driver_test_.Connect<fuchsia_hardware_badblock::Service::BadBlock>(partition_name);
    EXPECT_OK(client_end);
    return fidl::WireClient<fuchsia_hardware_badblock::BadBlock>(
        std::move(client_end.value()),
        driver_test_.runtime().GetForegroundDispatcher()->async_dispatcher());
  }

  zx::result<std::vector<uint32_t>> GetBadBlockList(
      fidl::WireClient<fuchsia_hardware_badblock::BadBlock>& client) {
    zx::result<std::vector<uint32_t>> result_out;
    client->GetBadBlockList().ThenExactlyOnce(
        [&](fidl::WireUnownedResult<fuchsia_hardware_badblock::BadBlock::GetBadBlockList>& result) {
          if (!result.ok()) {
            result_out = zx::error(result.status());
          } else if (result->is_error()) {
            result_out = zx::error(result->error_value());
          } else {
            const auto& list = result->value()->bad_block_list;
            result_out = zx::ok(std::vector<uint32_t>(list.begin(), list.end()));
          }
          driver_test_.runtime().Quit();
        });
    driver_test_.runtime().Run();
    driver_test_.runtime().ResetQuit();
    return result_out;
  }

  zx::result<> MarkBlockBad(fidl::WireClient<fuchsia_hardware_badblock::BadBlock>& client,
                            uint32_t block) {
    zx::result<> result_out;
    client->MarkBlockBad(block).ThenExactlyOnce(
        [&](fidl::WireUnownedResult<fuchsia_hardware_badblock::BadBlock::MarkBlockBad>& result) {
          if (!result.ok()) {
            result_out = zx::error(result.status());
          } else if (result->is_error()) {
            result_out = zx::error(result->error_value());
          } else {
            result_out = zx::ok();
          }
          driver_test_.runtime().Quit();
        });
    driver_test_.runtime().Run();
    driver_test_.runtime().ResetQuit();
    return result_out;
  }

 protected:
  fdf_testing::ForegroundDriverTest<FixtureConfig> driver_test_;
};

// Verify that the nandpart driver creates a nandpart device when given a single partition.
TEST_F(NandpartTest, OnePartition) {
  static const fuchsia_hardware_nand::Config kNandConfig(
      {.bad_block_config = fuchsia_hardware_nand::BadBlockConfig({
           .type = fuchsia_hardware_nand::BadBlockConfigType::kAmlogicUboot,
           .table_start_block = 96,
           .table_end_block = 99,
       })});

  static const fuchsia_boot_metadata::PartitionMap kPartitionMap(
      {.block_count = 1,
       .block_size = 16384,
       .partitions = std::vector{{fuchsia_boot_metadata::Partition({
           .first_block = 0,
           .last_block = 0,
           .name = "partition 1",
       })}}});

  static const nand_info_t kNandInfo = {
      .page_size = 4096,
      .pages_per_block = 4,
      .num_blocks = 100,
      .ecc_bits = 0,
      .oob_size = 8,
  };

  StartDriver(kNandConfig, kPartitionMap, kNandInfo);

  // Verify that the nandpart driver created a new nandpart device that serves the nand banjo
  // protocol and bad block FIDL service.
  ConnectToBanjo<ddk::NandProtocolClient>("partition 1");
  auto bad_block = ConnectToBadBlock("partition 1");
  zx::result bad_block_list = GetBadBlockList(bad_block);
  ASSERT_OK(bad_block_list);
  EXPECT_TRUE(bad_block_list->empty());
}

TEST_F(NandpartTest, MultiPartitionBadBlock) {
  static const fuchsia_hardware_nand::Config kNandConfig(
      {.bad_block_config = fuchsia_hardware_nand::BadBlockConfig({
           .type = fuchsia_hardware_nand::BadBlockConfigType::kAmlogicUboot,
           .table_start_block = 96,
           .table_end_block = 99,
       })});

  static const fuchsia_boot_metadata::PartitionMap kPartitionMap(
      {.block_count = 20,
       .block_size = 16384,
       .partitions = std::vector{{
           fuchsia_boot_metadata::Partition({
               .first_block = 0,
               .last_block = 9,
               .name = "partition-0",
           }),
           fuchsia_boot_metadata::Partition({
               .first_block = 10,
               .last_block = 19,
               .name = "partition-1",
           }),
       }}});

  static const nand_info_t kNandInfo = {
      .page_size = 4096,
      .pages_per_block = 4,
      .num_blocks = 100,
      .ecc_bits = 0,
      .oob_size = 8,
  };

  StartDriver(kNandConfig, kPartitionMap, kNandInfo);

  auto bad_block_0 = ConnectToBadBlock("partition-0");
  auto bad_block_1 = ConnectToBadBlock("partition-1");

  // Initially, both partitions should have an empty bad block list.
  {
    zx::result list_0 = GetBadBlockList(bad_block_0);
    ASSERT_OK(list_0);
    EXPECT_TRUE(list_0->empty());

    zx::result list_1 = GetBadBlockList(bad_block_1);
    ASSERT_OK(list_1);
    EXPECT_TRUE(list_1->empty());
  }

  // Out of range check on partition-0: num_blocks = 10 (blocks 0..9). Block 10 is out of bounds.
  EXPECT_STATUS(MarkBlockBad(bad_block_0, 10).status_value(), ZX_ERR_OUT_OF_RANGE);

  // Out of range check on partition-1: num_blocks = 10 (blocks 0..9). Block 10 is out of bounds.
  EXPECT_STATUS(MarkBlockBad(bad_block_1, 10).status_value(), ZX_ERR_OUT_OF_RANGE);

  // Mark block 2 as bad on partition-0.
  EXPECT_OK(MarkBlockBad(bad_block_0, 2));

  // Mark block 3 as bad on partition-1 (translates to 10 + 3 = 13 on underlying nand).
  EXPECT_OK(MarkBlockBad(bad_block_1, 3));

  // Verify partition-relative bad block list on partition-0: should only contain block 2.
  {
    zx::result list_0 = GetBadBlockList(bad_block_0);
    ASSERT_OK(list_0);
    ASSERT_EQ(list_0->size(), 1u);
    EXPECT_EQ((*list_0)[0], 2u);
  }

  // Verify partition-relative bad block list on partition-1: should only contain block 3.
  {
    zx::result list_1 = GetBadBlockList(bad_block_1);
    ASSERT_OK(list_1);
    ASSERT_EQ(list_1->size(), 1u);
    EXPECT_EQ((*list_1)[0], 3u);
  }
}

TEST_F(NandpartTest, BoundsCheck) {
  static const fuchsia_hardware_nand::Config kNandConfig(
      {.bad_block_config = fuchsia_hardware_nand::BadBlockConfig({
           .type = fuchsia_hardware_nand::BadBlockConfigType::kAmlogicUboot,
           .table_start_block = 0,
           .table_end_block = 0,
       })});

  // These settings are strange, but are built so that size_bytes==num_pages==num_blocks.
  static const fuchsia_boot_metadata::PartitionMap kPartitionMap(
      {.block_count = 10,
       .block_size = 1,
       .partitions = std::vector{{fuchsia_boot_metadata::Partition({
           .first_block = 0,
           .last_block = 9,
           .name = "partition 1",
       })}}});

  StartDriver(kNandConfig, kPartitionMap);

  auto client = ConnectToBanjo<ddk::NandProtocolClient>("partition 1");

  nand_info_t info;
  size_t op_size;
  client.Query(&info, &op_size);

  auto TestOpRange = [&](uint32_t command, uint32_t offset, uint32_t length,
                         zx_status_t expected_status) {
    std::vector<uint8_t> mem(op_size);
    nand_operation_t* op = reinterpret_cast<nand_operation_t*>(mem.data());
    op->command = command;
    if (command == NAND_OP_ERASE) {
      op->erase.first_block = offset;
      op->erase.num_blocks = length;
    } else if (command == NAND_OP_READ_BYTES || command == NAND_OP_WRITE_BYTES) {
      op->rw_bytes.offset_nand = offset;
      op->rw_bytes.length = length;
    } else {
      op->rw.offset_nand = offset;
      op->rw.length = length;
    }

    struct CompletionInfo {
      sync_completion_t completion;
      zx_status_t status;
    } comp_info;
    sync_completion_reset(&comp_info.completion);

    auto callback = [](void* cookie, zx_status_t status, nand_operation_t* op) {
      auto* info = static_cast<CompletionInfo*>(cookie);
      info->status = status;
      sync_completion_signal(&info->completion);
    };

    client.Queue(op, callback, &comp_info);
    ASSERT_OK(sync_completion_wait(&comp_info.completion, ZX_SEC(5)));
    EXPECT_STATUS(comp_info.status, expected_status);
  };

  for (uint32_t op_type :
       {NAND_OP_READ, NAND_OP_WRITE, NAND_OP_ERASE, NAND_OP_READ_BYTES, NAND_OP_WRITE_BYTES}) {
    // In bounds:
    TestOpRange(op_type, 0, 1, ZX_OK);
    TestOpRange(op_type, 9, 1, ZX_OK);
    TestOpRange(op_type, 0, 10, ZX_OK);

    // Out of bounds:
    TestOpRange(op_type, 10, 1, ZX_ERR_OUT_OF_RANGE);
    TestOpRange(op_type, 9, 2, ZX_ERR_OUT_OF_RANGE);
    TestOpRange(op_type, 0, 11, ZX_ERR_OUT_OF_RANGE);

    // Overflow:
    TestOpRange(op_type, 0xFFFFFFFF, 1, ZX_ERR_OUT_OF_RANGE);
    TestOpRange(op_type, 1, 0xFFFFFFFF, ZX_ERR_OUT_OF_RANGE);
  }
}

}  // namespace nand::testing
