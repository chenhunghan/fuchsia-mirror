// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/magma_service/mock/mock_bus_mapper.h>
#include <lib/magma_service/sys_driver/magma_system_buffer.h>
#include <lib/magma_service/sys_driver/magma_system_semaphore.h>

#include <limits>

#include <gtest/gtest.h>

#include "src/graphics/drivers/msd-vsi-vip/src/address_space.h"
#include "src/graphics/drivers/msd-vsi-vip/src/command_buffer.h"
#include "src/graphics/drivers/msd-vsi-vip/src/mapped_batch.h"
#include "src/graphics/drivers/msd-vsi-vip/src/msd_vsi_context.h"
#include "src/graphics/drivers/msd-vsi-vip/src/msd_vsi_driver.h"

// Holds the buffers and semaphores associated with a fake test batch.
class BatchData {
 public:
  static std::pair<std::unique_ptr<MappedBatch>, std::unique_ptr<BatchData>> Create(
      std::shared_ptr<MsdVsiContext> context, uint32_t num_resources) {
    auto driver = std::make_unique<MsdVsiDriver>();
    if (!driver)
      return {};

    std::vector<std::unique_ptr<msd::MagmaSystemSemaphore>> signal_semaphores;
    std::vector<std::unique_ptr<msd::MagmaSystemBuffer>> resources;

    for (unsigned int i = 0; i < num_resources; i++) {
      auto buffer = magma::PlatformBuffer::Create(kResourceSize, "test buffer");
      if (!buffer)
        return {};
      resources.emplace_back(msd::MagmaSystemBuffer::Create(driver.get(), std::move(buffer)));
    }

    for (unsigned int i = 0; i < kNumSignalSemaphores; i++) {
      auto semaphore = magma::PlatformSemaphore::Create();
      if (!semaphore)
        return {};

      uint32_t handle;
      if (!semaphore->duplicate_handle(&handle))
        return {};

      signal_semaphores.emplace_back(msd::MagmaSystemSemaphore::Create(
          driver.get(), zx::event(handle), semaphore->global_id(), /*flags=*/0));
    }

    auto batch_data =
        std::make_unique<BatchData>(std::move(signal_semaphores), std::move(resources));

    return {batch_data->CreateBatch(context), std::move(batch_data)};
  }

  static constexpr uint64_t kResourceSize = 4096;
  static constexpr uint32_t kNumSignalSemaphores = 3;

  BatchData(std::vector<std::unique_ptr<msd::MagmaSystemSemaphore>> signal_semaphores,
            std::vector<std::unique_ptr<msd::MagmaSystemBuffer>> resources)
      : signal_semaphores_(std::move(signal_semaphores)), resources_(std::move(resources)) {}

 private:
  // Returns a new batch created from the BatchData.
  std::unique_ptr<MappedBatch> CreateBatch(std::shared_ptr<MsdVsiContext> context) {
    auto command_buffer = msd::magma_command_buffer{
        .resource_count = static_cast<uint32_t>(resources_.size()),
        .batch_buffer_resource_index = 0,
        .batch_start_offset = 0,
        .wait_semaphore_count = 0,
        .signal_semaphore_count = static_cast<uint32_t>(signal_semaphores_.size()),
        .flags = 0,
    };
    std::vector<magma_exec_resource> resources;
    std::vector<msd::Buffer*> msd_buffers;
    for (auto& buf : resources_) {
      resources.push_back({
          .buffer_id = buf->platform_buffer()->id(),
          .offset = 0,
          .length = kResourceSize,
      });
      msd_buffers.push_back(buf->msd_buf());
    }
    std::vector<msd::Semaphore*> msd_signal_semaphores;
    for (const auto& semaphore : signal_semaphores_) {
      msd_signal_semaphores.emplace_back(semaphore->msd_semaphore());
    }
    return MsdVsiContext::CreateBatch(context, &command_buffer, resources.data(),
                                      msd_buffers.data(), nullptr, msd_signal_semaphores.data());
  }

  std::vector<std::unique_ptr<msd::MagmaSystemSemaphore>> signal_semaphores_;
  std::vector<std::unique_ptr<msd::MagmaSystemBuffer>> resources_;
};

class TestMsdVsiContext : public ::testing::Test {
 public:
  TestMsdVsiContext() : mock_connection_owner_(&ringbuffer_) {}

  void SetUp() {
    static constexpr uint32_t kAddressSpaceIndex = 1;
    address_space_ = AddressSpace::Create(&mock_address_space_owner_, kAddressSpaceIndex);
    EXPECT_NE(address_space_, nullptr);

    // Buffers are not submitted to hardware, so we just need a mock ringbuffer that the
    // context can map.
    const uint32_t kRingbufferSize = magma::page_size();
    ringbuffer_ = std::make_unique<Ringbuffer>(MsdVsiBuffer::Create(kRingbufferSize, "ringbuffer"));
    EXPECT_NE(ringbuffer_, nullptr);

    connection_ =
        MsdVsiConnection::Create(&mock_connection_owner_, address_space_, 0 /* client_id */);
    EXPECT_NE(connection_, nullptr);

    context_ = MsdVsiContext::Create(connection_, address_space_, ringbuffer_.get());
    EXPECT_NE(context_, nullptr);
  }

 protected:
  class MockConnectionOwner : public MsdVsiConnection::Owner {
   public:
    explicit MockConnectionOwner(std::unique_ptr<Ringbuffer>* ringbuffer)
        : ringbuffer_(ringbuffer) {}

    // MsdVsiConnection::Owner
    Ringbuffer* GetRingbuffer() override { return ringbuffer_->get(); }

    magma::Status SubmitBatch(std::unique_ptr<MappedBatch> batch, bool do_flush) override {
      submitted_batch_ids_.push_back(batch->GetBatchBufferId());
      if (submitted_batch_ids_.size() == num_expected_batches_) {
        finished_semaphore_->Signal();
      }
      return MAGMA_STATUS_OK;
    }

    // Signals |finished_semaphore| once |num_expected_batches| are received.
    void SetSignalOnCompletion(uint32_t num_expected_batches,
                               std::shared_ptr<magma::PlatformSemaphore> finished_semaphore) {
      submitted_batch_ids_.clear();
      num_expected_batches_ = num_expected_batches;
      finished_semaphore_ = std::move(finished_semaphore);
    }

    std::vector<uint64_t> GetSubmittedBatchIds() { return submitted_batch_ids_; }

   private:
    std::unique_ptr<Ringbuffer>* ringbuffer_;
    std::vector<uint64_t> submitted_batch_ids_;
    uint32_t num_expected_batches_ = 0;
    std::shared_ptr<magma::PlatformSemaphore> finished_semaphore_;
  };

  class MockAddressSpaceOwner : public AddressSpace::Owner {
   public:
    MockAddressSpaceOwner() : bus_mapper_((1ul << (20 - 1))) {}

    magma::PlatformBusMapper* GetBusMapper() override { return &bus_mapper_; }

    void AddressSpaceReleased(AddressSpace* address_space) override {}

   private:
    MockBusMapper bus_mapper_;
  };

  // Submits the requested number of batches and verifies that the connection
  // owner receives the same batches.
  void TestSubmitBatches(uint32_t num_batches, uint32_t num_resources_per_batch);

  MockConnectionOwner mock_connection_owner_;
  MockAddressSpaceOwner mock_address_space_owner_;
  std::shared_ptr<AddressSpace> address_space_;
  std::shared_ptr<MsdVsiConnection> connection_;
  std::shared_ptr<MsdVsiContext> context_;

  std::unique_ptr<Ringbuffer> ringbuffer_;
};

void TestMsdVsiContext::TestSubmitBatches(uint32_t num_batches, uint32_t num_resources_per_batch) {
  auto finished_semaphore =
      std::shared_ptr<magma::PlatformSemaphore>(magma::PlatformSemaphore::Create());
  mock_connection_owner_.SetSignalOnCompletion(num_batches, finished_semaphore);

  // Submit the batches and save the batch ids.
  std::vector<std::unique_ptr<BatchData>> batch_data;
  std::vector<uint64_t> batch_ids;
  for (unsigned int i = 0; i < num_batches; i++) {
    auto [batch, data] = BatchData::Create(context_, num_resources_per_batch);
    ASSERT_NE(batch, nullptr);

    ASSERT_EQ(batch->IsCommandBuffer(), num_resources_per_batch > 0);

    batch_data.push_back(std::move(data));
    batch_ids.push_back(batch->GetBatchBufferId());

    context_->SubmitBatch(std::move(batch));
  }

  // Wait for the batches to be received.
  constexpr uint64_t kTimeoutMs = 1000;
  ASSERT_EQ(MAGMA_STATUS_OK, finished_semaphore->Wait(kTimeoutMs).get());

  // Check the correct batch ids were received.
  auto submitted_batch_ids = mock_connection_owner_.GetSubmittedBatchIds();
  ASSERT_EQ(submitted_batch_ids.size(), batch_data.size());
  for (unsigned int i = 0; i < batch_data.size(); i++) {
    ASSERT_EQ(batch_ids[i], submitted_batch_ids[i]);
  }
}

TEST_F(TestMsdVsiContext, SubmitBatchesNoResources) {
  ASSERT_NO_FATAL_FAILURE(TestSubmitBatches(2 /* num_batches */, 0 /* num_resources_per_batch */));
}

TEST_F(TestMsdVsiContext, SubmitBatchesWithResources) {
  ASSERT_NO_FATAL_FAILURE(TestSubmitBatches(5 /* num_batches */, 2 /* num_resources_per_batch */));
}

TEST_F(TestMsdVsiContext, BatchBufferWriteOffsetWithResourceOffset) {
  const uint64_t kPageSize = magma::page_size();
  std::shared_ptr<MsdVsiBuffer> buffer = MsdVsiBuffer::Create(kPageSize * 2, "batch");
  ASSERT_NE(buffer, nullptr);

  ASSERT_TRUE(
      connection_->MapBufferGpu(buffer, 0x10000, 1 /* page_offset */, 1 /* page_count */).ok());

  auto cmd_buf = std::make_unique<msd::magma_command_buffer>(msd::magma_command_buffer{
      .resource_count = 1,
      .batch_buffer_resource_index = 0,
      .batch_start_offset = 80,
      .wait_semaphore_count = 0,
      .signal_semaphore_count = 0,
      .flags = 0,
  });
  std::vector<CommandBuffer::ExecResource> resources;
  resources.emplace_back(
      CommandBuffer::ExecResource{.buffer = buffer, .offset = kPageSize, .length = 12});

  auto batch = CommandBuffer::Create(context_, 0, std::move(cmd_buf), std::move(resources), {});
  ASSERT_NE(batch, nullptr);
  ASSERT_TRUE(batch->PrepareForExecution());
  EXPECT_TRUE(batch->IsValidBatch());

  auto write_offset = batch->GetBatchBufferWriteOffset();
  ASSERT_TRUE(write_offset.has_value());
  EXPECT_EQ(*write_offset, kPageSize + 80 + 16);
}

TEST_F(TestMsdVsiContext, BatchBufferWriteOffsetOverflow) {
  const uint64_t kPageSize = magma::page_size();
  const uint64_t kOverflowOffset = std::numeric_limits<uint64_t>::max() - (kPageSize - 1);

  std::shared_ptr<MsdVsiBuffer> buffer = MsdVsiBuffer::Create(kPageSize * 2, "batch");
  ASSERT_NE(buffer, nullptr);

  auto bus_mapping = mock_address_space_owner_.GetBusMapper()->MapPageRangeBus(
      buffer->platform_buffer(), 0, 2, true);
  ASSERT_NE(bus_mapping, nullptr);
  ASSERT_TRUE(address_space_->Insert(0x10000, bus_mapping.get(), 0));
  auto mapping = std::make_shared<GpuMapping>(address_space_, buffer, kOverflowOffset,
                                              kPageSize * 2, 0x10000, std::move(bus_mapping));
  ASSERT_TRUE(address_space_->AddMapping(mapping));

  auto cmd_buf = std::make_unique<msd::magma_command_buffer>(msd::magma_command_buffer{
      .resource_count = 1,
      .batch_buffer_resource_index = 0,
      .batch_start_offset = kPageSize - 8,
      .wait_semaphore_count = 0,
      .signal_semaphore_count = 0,
      .flags = 0,
  });
  std::vector<CommandBuffer::ExecResource> resources;
  resources.emplace_back(
      CommandBuffer::ExecResource{.buffer = buffer, .offset = kOverflowOffset, .length = 8});

  auto batch = CommandBuffer::Create(context_, 0, std::move(cmd_buf), std::move(resources), {});
  ASSERT_NE(batch, nullptr);
  ASSERT_TRUE(batch->PrepareForExecution());
  EXPECT_EQ(batch->GetBatchBufferWriteOffset(), std::nullopt);
  EXPECT_FALSE(batch->IsValidBatch());

  // Also verify unaligned length near UINT64_MAX does not wrap around to 0 when aligning up.
  auto cmd_buf_len_overflow = std::make_unique<msd::magma_command_buffer>(msd::magma_command_buffer{
      .resource_count = 1,
      .batch_buffer_resource_index = 0,
      .batch_start_offset = 0,
      .wait_semaphore_count = 0,
      .signal_semaphore_count = 0,
      .flags = 0,
  });
  std::vector<CommandBuffer::ExecResource> len_overflow_resources;
  len_overflow_resources.emplace_back(CommandBuffer::ExecResource{
      .buffer = buffer,
      .offset = 0,
      .length = std::numeric_limits<uint64_t>::max() - 3,
  });
  auto batch_len_overflow = CommandBuffer::Create(context_, 0, std::move(cmd_buf_len_overflow),
                                                  std::move(len_overflow_resources), {});
  ASSERT_NE(batch_len_overflow, nullptr);
  EXPECT_EQ(batch_len_overflow->GetBatchBufferWriteOffset(), std::nullopt);
}

TEST_F(TestMsdVsiContext, ContextStateBufferOffsetOverflow) {
  const uint64_t kPageSize = magma::page_size();
  const uint64_t kOverflowOffset = std::numeric_limits<uint64_t>::max() - (kPageSize - 1);

  std::shared_ptr<MsdVsiBuffer> batch_buf = MsdVsiBuffer::Create(kPageSize, "batch");
  ASSERT_NE(batch_buf, nullptr);
  ASSERT_TRUE(
      connection_->MapBufferGpu(batch_buf, 0x10000, 0 /* page_offset */, 1 /* page_count */).ok());

  std::shared_ptr<MsdVsiBuffer> csb_buf = MsdVsiBuffer::Create(kPageSize, "csb");
  ASSERT_NE(csb_buf, nullptr);
  auto csb_bus_mapping = mock_address_space_owner_.GetBusMapper()->MapPageRangeBus(
      csb_buf->platform_buffer(), 0, 1, true);
  ASSERT_NE(csb_bus_mapping, nullptr);
  ASSERT_TRUE(address_space_->Insert(0x20000, csb_bus_mapping.get(), 0));
  auto csb_mapping = std::make_shared<GpuMapping>(address_space_, csb_buf, kOverflowOffset,
                                                  kPageSize, 0x20000, std::move(csb_bus_mapping));
  ASSERT_TRUE(address_space_->AddMapping(csb_mapping));

  auto cmd_buf = std::make_unique<msd::magma_command_buffer>(msd::magma_command_buffer{
      .resource_count = 2,
      .batch_buffer_resource_index = 0,
      .batch_start_offset = 0,
      .wait_semaphore_count = 0,
      .signal_semaphore_count = 0,
      .flags = 0,
  });
  std::vector<CommandBuffer::ExecResource> resources;
  resources.emplace_back(
      CommandBuffer::ExecResource{.buffer = batch_buf, .offset = 0, .length = 8});
  resources.emplace_back(CommandBuffer::ExecResource{
      .buffer = csb_buf,
      .offset = kOverflowOffset,
      .length = kPageSize - 8,
  });

  auto batch = CommandBuffer::Create(context_, 0, std::move(cmd_buf), std::move(resources), {});
  ASSERT_NE(batch, nullptr);
  ASSERT_TRUE(batch->PrepareForExecution());
  EXPECT_FALSE(batch->IsValidBatch());
}
