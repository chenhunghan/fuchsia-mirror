// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/drivers/nvme/queue-pair.h"

#include <lib/driver/mmio/testing/cpp/test-helper.h>
#include <lib/driver/testing/cpp/scoped_global_logger.h>
#include <lib/fake-bti/bti.h>
#include <lib/mmio-ptr/fake.h>
#include <lib/sync/completion.h>
#include <lib/zx/bti.h>
#include <lib/zx/vmo.h>

#include <vector>

#include <gtest/gtest.h>

#include "src/devices/block/drivers/nvme/commands.h"
#include "src/devices/block/drivers/nvme/io-command.h"
#include "src/devices/block/drivers/nvme/registers.h"
#include "src/lib/testing/predicates/status.h"

namespace nvme {

class QueuePairTest : public ::testing::Test {
 public:
  void SetUp() override { ASSERT_OK(fake_bti_create(fake_bti_.reset_and_get_address())); }

 protected:
  TransactionData& txn(QueuePair* pair, size_t id) __TA_NO_THREAD_SAFETY_ANALYSIS {
    return pair->txns_[id];
  }

  // Returns the size in bytes of each VMO range currently pinned through |fake_bti_|, in the order
  // in which they were pinned. This includes the buffers that back the queues themselves.
  std::vector<uint64_t> PinnedSizes() {
    size_t count = 0;
    EXPECT_OK(fake_bti_get_pinned_vmos(fake_bti_.get(), nullptr, 0, &count));
    std::vector<fake_bti_pinned_vmo_info_t> infos(count);
    EXPECT_OK(fake_bti_get_pinned_vmos(fake_bti_.get(), infos.data(), infos.size(), &count));
    std::vector<uint64_t> sizes;
    for (const fake_bti_pinned_vmo_info_t& info : infos) {
      // Close the duplicated VMO handle.
      zx::vmo vmo(info.vmo);
      sizes.push_back(info.size);
    }
    return sizes;
  }

  zx::bti fake_bti_;
  // We only use the capability register for the doorbell stride, so 0 is fine.
  CapabilityReg caps_ = CapabilityReg::Get().FromValue(0);
  fdf::MmioBuffer mmio_ = fdf_testing::CreateMmioBuffer(
      NVME_REG_DOORBELL_BASE * 0x100, ZX_CACHE_POLICY_UNCACHED_DEVICE, &kMmioOps, this);

  // void doorbell_ring(bool is_completion, size_t queue_id, uint16_t value)
  std::function<void(bool, size_t, uint16_t)> doorbell_ring_;

 private:
  static void Write32(const void* ctx, const fdf::MmioBuffer& mmio, uint32_t val, zx_off_t offs) {
    offs -= NVME_REG_DOORBELL_BASE;
    offs /= 4;
    bool completion_doorbell = (offs & 1);
    size_t queue_id = offs / 2;
    static_cast<const QueuePairTest*>(ctx)->doorbell_ring_(completion_doorbell, queue_id,
                                                           val & 0xffff);
  }

  static uint32_t Read32(const void* ctx, const fdf::MmioBuffer& mmio, zx_off_t offs) {
    ZX_ASSERT(false);
  }

// Define read/write for |bits| that just crashes.
#define STUB_IO_OP(bits)                                                                          \
  static void Write##bits(const void* ctx, const fdf::MmioBuffer& mmio, uint##bits##_t val,       \
                          zx_off_t offs) {                                                        \
    ZX_ASSERT(false);                                                                             \
  }                                                                                               \
  static uint##bits##_t Read##bits(const void* ctx, const fdf::MmioBuffer& mmio, zx_off_t offs) { \
    ZX_ASSERT(false);                                                                             \
  }

  STUB_IO_OP(64)
  STUB_IO_OP(16)
  STUB_IO_OP(8)
#undef STUB_IO_OP

  static constexpr fdf::MmioBufferOps kMmioOps = {
      .Read8 = Read8,
      .Read16 = Read16,
      .Read32 = Read32,
      .Read64 = Read64,
      .Write8 = Write8,
      .Write16 = Write16,
      .Write32 = Write32,
      .Write64 = Write64,
  };

  fdf_testing::ScopedGlobalLogger logger_;
};

TEST_F(QueuePairTest, TestSubmit) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 0, 100, caps_, mmio_, /*prealloc_prp=*/false);
  ASSERT_OK(pair.status_value());

  sync_completion_t rang;
  size_t doorbell_ring_count = 0;
  doorbell_ring_ = [&rang, &pair, &doorbell_ring_count](bool is_completion, size_t queue_id,
                                                        uint16_t value) {
    ASSERT_FALSE(is_completion);
    ASSERT_EQ(0u, queue_id);
    ASSERT_EQ(doorbell_ring_count + 1, value);
    doorbell_ring_count++;
    Submission* submissions = static_cast<Submission*>(pair->submission().head());
    ASSERT_EQ(0x9fu, submissions[value - 1].opcode());
    sync_completion_signal(&rang);
  };

  Submission s(0x9f);
  ASSERT_OK(pair->Submit(s, std::nullopt, 0, 0));

  sync_completion_wait(&rang, ZX_TIME_INFINITE);

  rang = {};
  s.set_opcode(0x9f);
  ASSERT_OK(pair->Submit(s, std::nullopt, 0, 0));
  sync_completion_wait(&rang, ZX_TIME_INFINITE);
}

TEST_F(QueuePairTest, TestCheckCompletionsNothingReady) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 0, 100, caps_, mmio_, /*prealloc_prp=*/false);
  ASSERT_OK(pair.status_value());
  Completion* completions = static_cast<Completion*>(pair->completion().head());
  memset(completions, 0, sizeof(*completions));

  doorbell_ring_ = [](bool, size_t, uint16_t) {
    ZX_ASSERT_MSG(false, "Doorbell should not have been rung");
  };

  Completion* completion = nullptr;
  ASSERT_EQ(pair->CheckForNewCompletion(&completion), ZX_ERR_SHOULD_WAIT);
}

TEST_F(QueuePairTest, TestCheckCompletionsOneReady) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 0, 100, caps_, mmio_, /*prealloc_prp=*/false);
  ASSERT_OK(pair.status_value());

  doorbell_ring_ = [](bool is_completion, size_t queue_id, uint16_t new_value) {
    ASSERT_FALSE(is_completion);
    ASSERT_EQ(0u, queue_id);
    ASSERT_EQ(1u, new_value);
  };
  Submission s(0);
  ASSERT_OK(pair->Submit(s, std::nullopt, 0, 0));

  Completion* completions = static_cast<Completion*>(pair->completion().head());
  memset(completions, 0, sizeof(*completions) * pair->completion().entry_count());
  completions[0].set_command_id(0);
  completions[0].set_phase(1);
  completions[0].set_sq_head(0);

  doorbell_ring_ = [](bool is_completion, size_t queue_id, uint16_t new_value) {
    ASSERT_TRUE(is_completion);
    ASSERT_EQ(0u, queue_id);
    ASSERT_EQ(1u, new_value);
  };

  Completion* completion = nullptr;
  ASSERT_EQ(pair->CheckForNewCompletion(&completion), ZX_OK);
  ASSERT_TRUE(completion->status_code_type() == StatusCodeType::kGeneric &&
              completion->status_code() == 0);
  pair->RingCompletionDb();
}

TEST_F(QueuePairTest, TestCheckCompletionsMultipleReady) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 0, 100, caps_, mmio_, /*prealloc_prp=*/false);
  ASSERT_OK(pair.status_value());

  uint16_t expected_doorbell = 1;
  doorbell_ring_ = [&expected_doorbell](bool is_completion, size_t queue_id, uint16_t new_value) {
    ASSERT_FALSE(is_completion);
    ASSERT_EQ(0u, queue_id);
    ASSERT_EQ(expected_doorbell, new_value);
    expected_doorbell++;
  };
  Submission s(0);
  {
    ASSERT_OK(pair->Submit(s, std::nullopt, 0, 0));
  }
  {
    ASSERT_OK(pair->Submit(s, std::nullopt, 0, 0));
  }

  Completion* completions = static_cast<Completion*>(pair->completion().head());
  memset(completions, 0, sizeof(*completions) * pair->completion().entry_count());
  completions[0].set_command_id(0);
  completions[0].set_phase(1);
  completions[0].set_sq_head(0);
  completions[1].set_command_id(1);
  completions[1].set_phase(1);
  completions[1].set_sq_head(1);

  // Expect only a single ring of the doorbell.
  doorbell_ring_ = [](bool is_completion, size_t queue_id, uint16_t new_value) {
    ASSERT_TRUE(is_completion);
    ASSERT_EQ(0u, queue_id);
    ASSERT_EQ(2, new_value);
  };

  Completion* completion = nullptr;
  ASSERT_EQ(pair->CheckForNewCompletion(&completion), ZX_OK);
  ASSERT_TRUE(completion->status_code_type() == StatusCodeType::kGeneric &&
              completion->status_code() == 0);
  ASSERT_EQ(pair->CheckForNewCompletion(&completion), ZX_OK);
  ASSERT_TRUE(completion->status_code_type() == StatusCodeType::kGeneric &&
              completion->status_code() == 0);
  pair->RingCompletionDb();
}

TEST_F(QueuePairTest, TestCheckCompletionsInactiveTransactionClearsIoCmd) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 0, 100, caps_, mmio_, /*prealloc_prp=*/false);
  ASSERT_OK(pair.status_value());

  doorbell_ring_ = [](bool, size_t, uint16_t) {};

  IoCommand cmd;
  Submission s(0);
  ASSERT_OK(pair->Submit(s, std::nullopt, 0, 0, &cmd));

  Completion* completions = static_cast<Completion*>(pair->completion().head());
  memset(completions, 0, sizeof(*completions) * pair->completion().entry_count());
  // First completion is valid for command_id 0.
  completions[0].set_command_id(0);
  completions[0].set_phase(1);
  completions[0].set_sq_head(0);

  // Second completion is duplicate/inactive for command_id 0.
  completions[1].set_command_id(0);
  completions[1].set_phase(1);
  completions[1].set_sq_head(0);

  Completion* completion = nullptr;
  IoCommand* io_cmd = nullptr;

  // 1st completion should succeed and return &cmd.
  ASSERT_EQ(pair->CheckForNewCompletion(&completion, &io_cmd), ZX_OK);
  EXPECT_EQ(io_cmd, &cmd);

  // 2nd completion is on an inactive transaction. It should return ZX_ERR_BAD_STATE and set
  // io_cmd to nullptr.
  EXPECT_EQ(pair->CheckForNewCompletion(&completion, &io_cmd), ZX_ERR_BAD_STATE);
  EXPECT_EQ(io_cmd, nullptr);
}

TEST_F(QueuePairTest, TestCheckCompletionsInvalidCommandId) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 0, 100, caps_, mmio_, /*prealloc_prp=*/false);
  ASSERT_OK(pair.status_value());

  doorbell_ring_ = [](bool, size_t, uint16_t) {};

  Completion* completions = static_cast<Completion*>(pair->completion().head());
  memset(completions, 0, sizeof(*completions) * pair->completion().entry_count());

  // Set command_id equal to txns_.size() (the off-by-one boundary).
  completions[0].set_command_id(static_cast<uint16_t>(pair->submission().entry_count()));
  completions[0].set_phase(1);
  completions[0].set_sq_head(0);

  // Set command_id way out of bounds.
  completions[1].set_command_id(0x9999);
  completions[1].set_phase(1);
  completions[1].set_sq_head(0);

  Completion* completion = nullptr;
  IoCommand dummy_cmd;
  IoCommand* io_cmd = &dummy_cmd;

  // Off-by-one boundary test: command_id == txns_.size()
  EXPECT_EQ(pair->CheckForNewCompletion(&completion, &io_cmd), ZX_ERR_BAD_STATE);
  EXPECT_EQ(io_cmd, nullptr);

  io_cmd = &dummy_cmd;
  // Way out of bounds test
  EXPECT_EQ(pair->CheckForNewCompletion(&completion, &io_cmd), ZX_ERR_BAD_STATE);
  EXPECT_EQ(io_cmd, nullptr);
}

TEST_F(QueuePairTest, TestSubmitRingWraparoundWithSlowTransaction) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 0, 4, caps_, mmio_, /*prealloc_prp=*/false);
  ASSERT_OK(pair.status_value());

  doorbell_ring_ = [](bool, size_t, uint16_t) {};

  IoCommand slow_cmd;
  IoCommand fast_cmd1;
  IoCommand fast_cmd2;
  IoCommand fast_cmd3;
  IoCommand wrapped_cmd;

  Submission s(0x02);
  // Submit slow_cmd at ring index 0 (CID 0) and fast_cmd1 at ring index 1 (CID 1).
  ASSERT_OK(pair->Submit(s, std::nullopt, 0, 0, &slow_cmd));
  ASSERT_OK(pair->Submit(s, std::nullopt, 0, 0, &fast_cmd1));

  Completion* completions = static_cast<Completion*>(pair->completion().head());
  memset(completions, 0, sizeof(*completions) * pair->completion().entry_count());

  // Complete fast_cmd1 (CID 1) out-of-order while slow_cmd (CID 0) remains in flight.
  // Controller reports sq_head = 2, freeing submission ring slots 0 and 1.
  completions[0].set_command_id(1);
  completions[0].set_phase(1);
  completions[0].set_sq_head(2);

  Completion* completion = nullptr;
  IoCommand* completed_cmd = nullptr;
  ASSERT_EQ(pair->CheckForNewCompletion(&completion, &completed_cmd), ZX_OK);
  EXPECT_EQ(completed_cmd, &fast_cmd1);

  // Submit fast_cmd2 (ring index 2) and fast_cmd3 (ring index 3), then complete both so sq_head
  // advances to 0 and submission_.NextIndex() wraps around to 0 while txns_[0] is still active.
  ASSERT_OK(pair->Submit(s, std::nullopt, 0, 0, &fast_cmd2));
  ASSERT_OK(pair->Submit(s, std::nullopt, 0, 0, &fast_cmd3));

  completions[1].set_command_id(2);
  completions[1].set_phase(1);
  completions[1].set_sq_head(3);
  completions[2].set_command_id(3);
  completions[2].set_phase(1);
  completions[2].set_sq_head(0);

  ASSERT_EQ(pair->CheckForNewCompletion(&completion, &completed_cmd), ZX_OK);
  EXPECT_EQ(completed_cmd, &fast_cmd2);
  ASSERT_EQ(pair->CheckForNewCompletion(&completion, &completed_cmd), ZX_OK);
  EXPECT_EQ(completed_cmd, &fast_cmd3);
  pair->RingCompletionDb();

  // Ring index is now back at 0 while txns_[0] (slow_cmd) is still active.
  // Submitting wrapped_cmd must allocate a free CID (CID 1) instead of returning
  // ZX_ERR_SHOULD_WAIT.
  ASSERT_EQ(pair->submission().NextIndex(), 0u);
  ASSERT_TRUE(txn(pair.value().get(), 0).active);
  ASSERT_OK(pair->Submit(s, std::nullopt, 0, 0, &wrapped_cmd));

  Submission* submissions = static_cast<Submission*>(pair->submission().head());
  EXPECT_EQ(submissions[0].cid(), 1u);

  // Complete wrapped_cmd (CID 1) and slow_cmd (CID 0, phase toggled after wrap at index 4).
  completions[3].set_command_id(1);
  completions[3].set_phase(1);
  completions[3].set_sq_head(1);
  completions[0].set_command_id(0);
  completions[0].set_phase(0);
  completions[0].set_sq_head(1);

  ASSERT_EQ(pair->CheckForNewCompletion(&completion, &completed_cmd), ZX_OK);
  EXPECT_EQ(completed_cmd, &wrapped_cmd);
  ASSERT_EQ(pair->CheckForNewCompletion(&completion, &completed_cmd), ZX_OK);
  EXPECT_EQ(completed_cmd, &slow_cmd);
}

TEST_F(QueuePairTest, TestSubmitWithDataOnePage) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 0, 100, caps_, mmio_, /*prealloc_prp=*/false);
  ASSERT_OK(pair.status_value());

  doorbell_ring_ = [](bool is_completion, size_t queue_id, uint16_t new_value) {
    ASSERT_FALSE(is_completion);
    ASSERT_EQ(0u, queue_id);
    ASSERT_EQ(1u, new_value);
  };
  zx::vmo data_vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &data_vmo));
  Submission s(0xa9);
  {
    ASSERT_OK(pair->Submit(s, data_vmo.borrow(), 0, zx_system_get_page_size()));
  }

  Submission* submitted = static_cast<Submission*>(pair->submission().head());
  ASSERT_EQ(0u, submitted->data_transfer_mode());
  ASSERT_EQ(0u, submitted->fused());
  ASSERT_EQ(0xa9u, submitted->opcode());
  ASSERT_EQ(uint64_t{FAKE_BTI_PHYS_ADDR}, submitted->data_pointer[0]);
  ASSERT_EQ(0u, submitted->data_pointer[1]);
  TransactionData& txn_data = txn(pair.value().get(), 0);
  ASSERT_FALSE(txn_data.prp_buffer);
  ASSERT_TRUE(txn_data.active);
}

TEST_F(QueuePairTest, TestSubmitWithDataTwoPages) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 0, 100, caps_, mmio_, /*prealloc_prp=*/false);
  ASSERT_OK(pair.status_value());

  doorbell_ring_ = [](bool is_completion, size_t queue_id, uint16_t new_value) {
    ASSERT_FALSE(is_completion);
    ASSERT_EQ(0u, queue_id);
    ASSERT_EQ(1u, new_value);
  };
  zx::vmo data_vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &data_vmo));
  Submission s(0xa9);
  {
    ASSERT_OK(pair->Submit(s, data_vmo.borrow(), 0, zx_system_get_page_size()));
  }

  Submission* submitted = static_cast<Submission*>(pair->submission().head());
  ASSERT_EQ(0u, submitted->data_transfer_mode());
  ASSERT_EQ(0u, submitted->fused());
  ASSERT_EQ(0xa9u, submitted->opcode());
  ASSERT_EQ(uint64_t{FAKE_BTI_PHYS_ADDR}, submitted->data_pointer[0]);
  ASSERT_EQ(0u, submitted->data_pointer[1]);
  TransactionData& txn_data = txn(pair.value().get(), 0);
  ASSERT_FALSE(txn_data.prp_buffer);
  ASSERT_TRUE(txn_data.active);
}

TEST_F(QueuePairTest, TestSubmitReportsCommandId) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 0, 100, caps_, mmio_, /*prealloc_prp=*/false);
  ASSERT_OK(pair.status_value());

  doorbell_ring_ = [](bool, size_t, uint16_t) {};

  Submission s(0);
  zx::result<uint16_t> cid = pair->Submit(s, std::nullopt, 0, 0);
  ASSERT_OK(cid);
  EXPECT_EQ(cid.value(), 0u);
  cid = pair->Submit(s, std::nullopt, 0, 0);
  ASSERT_OK(cid);
  EXPECT_EQ(cid.value(), 1u);

  Submission* submissions = static_cast<Submission*>(pair->submission().head());
  EXPECT_EQ(submissions[1].cid(), cid.value());
  EXPECT_EQ(pair->in_flight_count(), 2u);
}

TEST_F(QueuePairTest, TestSubmitWithMisalignedDataOffset) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 0, 100, caps_, mmio_, /*prealloc_prp=*/false);
  ASSERT_OK(pair.status_value());

  doorbell_ring_ = [](bool, size_t, uint16_t) {
    ZX_ASSERT_MSG(false, "Doorbell should not have been rung");
  };
  zx::vmo data_vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &data_vmo));
  const size_t pinned_count = PinnedSizes().size();

  // The offset portion of PRP1 must be dword aligned.
  Submission s(0xa9);
  for (zx_off_t vmo_offset : {1, 2, 3, 513}) {
    EXPECT_STATUS(pair->Submit(s, data_vmo.borrow(), vmo_offset, 512), ZX_ERR_INVALID_ARGS)
        << "vmo_offset=" << vmo_offset;
  }
  // The failed submissions must not have had any effect.
  EXPECT_EQ(pair->submission().NextIndex(), 0u);
  EXPECT_EQ(pair->in_flight_count(), 0u);
  EXPECT_FALSE(txn(pair.value().get(), 0).active);
  EXPECT_EQ(PinnedSizes().size(), pinned_count);

  // An offset that is dword aligned, but not page aligned, is accepted and passed on in PRP1.
  doorbell_ring_ = [](bool, size_t, uint16_t) {};
  ASSERT_OK(pair->Submit(s, data_vmo.borrow(), 516, 512));
  Submission* submitted = static_cast<Submission*>(pair->submission().head());
  EXPECT_EQ(submitted->data_pointer[0], uint64_t{FAKE_BTI_PHYS_ADDR} + 516);
  EXPECT_EQ(submitted->data_pointer[1], 0u);
  EXPECT_EQ(pair->in_flight_count(), 1u);
}

TEST_F(QueuePairTest, TestSubmitWithInvalidTransferSize) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 1, 100, caps_, mmio_, /*prealloc_prp=*/true);
  ASSERT_OK(pair.status_value());

  doorbell_ring_ = [](bool, size_t, uint16_t) {
    ZX_ASSERT_MSG(false, "Doorbell should not have been rung");
  };

  const size_t page_size = zx_system_get_page_size();
  const size_t max_transfer_bytes = QueuePair::kMaxTransferPages * page_size;
  zx::vmo data_vmo;
  ASSERT_OK(zx::vmo::create(max_transfer_bytes, 0, &data_vmo));
  const size_t pinned_count = PinnedSizes().size();

  struct TestCase {
    zx_off_t vmo_offset;
    size_t bytes;
  };
  const TestCase kTestCases[] = {
      // An empty transfer, which must not pin the page that a non-zero offset falls in.
      {.vmo_offset = 0, .bytes = 0},
      {.vmo_offset = 512, .bytes = 0},
      // Transfers that are larger than the maximum, including a page-aligned transfer that would
      // fit in kMaxPinnedPages pages.
      {.vmo_offset = 0, .bytes = max_transfer_bytes + 512},
      {.vmo_offset = 0, .bytes = max_transfer_bytes + page_size},
      {.vmo_offset = 512, .bytes = max_transfer_bytes + page_size},
      // Transfer sizes for which calculating the number of pages touched would overflow.
      {.vmo_offset = 0, .bytes = SIZE_MAX},
      {.vmo_offset = 512, .bytes = SIZE_MAX - 511},
  };

  IoCommand cmd;
  Submission s(0xa9);
  for (const TestCase& test_case : kTestCases) {
    EXPECT_STATUS(pair->Submit(s, data_vmo.borrow(), test_case.vmo_offset, test_case.bytes, &cmd),
                  ZX_ERR_INVALID_ARGS)
        << "vmo_offset=" << test_case.vmo_offset << " bytes=" << test_case.bytes;
  }
  // The failed submissions must not have had any effect.
  EXPECT_EQ(pair->submission().NextIndex(), 0u);
  EXPECT_EQ(pair->in_flight_count(), 0u);
  EXPECT_FALSE(txn(pair.value().get(), 0).active);
  EXPECT_EQ(PinnedSizes().size(), pinned_count);

  // A page-aligned transfer of exactly the maximum size is accepted.
  doorbell_ring_ = [](bool, size_t, uint16_t) {};
  ASSERT_OK(pair->Submit(s, data_vmo.borrow(), 0, max_transfer_bytes, &cmd));
  EXPECT_EQ(pair->in_flight_count(), 1u);
  EXPECT_EQ(PinnedSizes().back(), max_transfer_bytes);
}

TEST_F(QueuePairTest, TestSubmitMaxTransferWithPageOffset) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 1, 100, caps_, mmio_, /*prealloc_prp=*/true);
  ASSERT_OK(pair.status_value());

  doorbell_ring_ = [](bool, size_t, uint16_t) {};

  const size_t page_size = zx_system_get_page_size();
  const size_t max_transfer_bytes = QueuePair::kMaxTransferPages * page_size;
  constexpr zx_off_t kVmoOffset = 512;
  zx::vmo data_vmo;
  ASSERT_OK(zx::vmo::create(max_transfer_bytes + page_size, 0, &data_vmo));

  IoCommand cmd;
  Submission s(0xa9);

  // A transfer of the maximum size that does not start on a page boundary touches one more page
  // than kMaxTransferPages, and must be accepted.
  ASSERT_OK(pair->Submit(s, data_vmo.borrow(), kVmoOffset, max_transfer_bytes, &cmd));
  EXPECT_EQ(pair->in_flight_count(), 1u);
  EXPECT_EQ(PinnedSizes().back(), QueuePair::kMaxPinnedPages * page_size);

  Submission* submitted = static_cast<Submission*>(pair->submission().head());
  TransactionData& txn_data = txn(pair.value().get(), 0);
  ASSERT_TRUE(txn_data.active);
  ASSERT_TRUE(txn_data.prp_buffer);
  // PRP1 is the first page plus the offset into it. PRP2 points at the list of the remaining pages,
  // which follows the first page's address in the PRP buffer.
  EXPECT_EQ(submitted->data_pointer[0], uint64_t{FAKE_BTI_PHYS_ADDR} + kVmoOffset);
  EXPECT_EQ(submitted->data_pointer[1], txn_data.prp_buffer->phys() + sizeof(uint64_t));
  uint64_t* prps = static_cast<uint64_t*>(txn_data.prp_buffer->virt());
  for (size_t i = 1; i < QueuePair::kMaxPinnedPages; ++i) {
    ASSERT_EQ(prps[i], uint64_t{FAKE_BTI_PHYS_ADDR}) << "PRP " << i << " had wrong value";
  }
}

TEST_F(QueuePairTest, TestCheckCompletionsReleasesTransaction) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 1, 100, caps_, mmio_, /*prealloc_prp=*/true);
  ASSERT_OK(pair.status_value());

  doorbell_ring_ = [](bool, size_t, uint16_t) {};

  zx::vmo data_vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &data_vmo));
  const size_t pinned_count = PinnedSizes().size();

  IoCommand cmd;
  Submission s(0xa9);
  ASSERT_OK(pair->Submit(s, data_vmo.borrow(), 0, zx_system_get_page_size(), &cmd));

  TransactionData& txn_data = txn(pair.value().get(), 0);
  EXPECT_TRUE(txn_data.active);
  EXPECT_TRUE(txn_data.pmt.is_valid());
  EXPECT_EQ(pair->in_flight_count(), 1u);
  EXPECT_EQ(PinnedSizes().size(), pinned_count + 1);

  Completion* completions = static_cast<Completion*>(pair->completion().head());
  memset(completions, 0, sizeof(*completions) * pair->completion().entry_count());
  completions[0].set_command_id(0);
  completions[0].set_phase(1);
  completions[0].set_sq_head(0);

  Completion* completion = nullptr;
  IoCommand* io_cmd = nullptr;
  ASSERT_OK(pair->CheckForNewCompletion(&completion, &io_cmd));
  EXPECT_EQ(completion, &completions[0]);
  EXPECT_EQ(io_cmd, &cmd);

  // The data must have been unpinned and the transaction must be available for reuse, with its
  // preallocated PRP buffer intact.
  EXPECT_FALSE(txn_data.active);
  EXPECT_FALSE(txn_data.pmt.is_valid());
  EXPECT_EQ(txn_data.io_cmd, nullptr);
  EXPECT_TRUE(txn_data.prp_buffer);
  EXPECT_EQ(pair->in_flight_count(), 0u);
  EXPECT_EQ(PinnedSizes().size(), pinned_count);
}

TEST_F(QueuePairTest, TestCancelAll) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 1, 100, caps_, mmio_, /*prealloc_prp=*/true);
  ASSERT_OK(pair.status_value());

  doorbell_ring_ = [](bool, size_t, uint16_t) {};

  zx::vmo data_vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &data_vmo));
  const size_t pinned_count = PinnedSizes().size();

  // Submit a command with data, a command without data, and a command with data that is not
  // associated with an IoCommand (like an admin command).
  IoCommand data_cmd;
  IoCommand no_data_cmd;
  Submission s(0xa9);
  ASSERT_OK(pair->Submit(s, data_vmo.borrow(), 0, zx_system_get_page_size(), &data_cmd));
  ASSERT_OK(pair->Submit(s, std::nullopt, 0, 0, &no_data_cmd));
  ASSERT_OK(pair->Submit(s, data_vmo.borrow(), 0, zx_system_get_page_size()));
  EXPECT_EQ(pair->in_flight_count(), 3u);
  EXPECT_EQ(PinnedSizes().size(), pinned_count + 2);

  std::vector<IoCommand*> cancelled;
  pair->CancelAll([&cancelled](IoCommand* io_cmd) { cancelled.push_back(io_cmd); });

  // Only transactions with an IoCommand are reported.
  EXPECT_EQ(cancelled, (std::vector<IoCommand*>{&data_cmd, &no_data_cmd}));
  EXPECT_EQ(pair->in_flight_count(), 0u);
  EXPECT_EQ(PinnedSizes().size(), pinned_count);
  for (size_t i = 0; i < 3; ++i) {
    TransactionData& txn_data = txn(pair.value().get(), i);
    EXPECT_FALSE(txn_data.active) << "transaction " << i;
    EXPECT_FALSE(txn_data.pmt.is_valid()) << "transaction " << i;
    EXPECT_EQ(txn_data.io_cmd, nullptr) << "transaction " << i;
  }

  // Nothing is in flight any more, so cancelling again reports nothing.
  pair->CancelAll([](IoCommand*) { ADD_FAILURE() << "Unexpected cancellation"; });

  // A completion for a cancelled transaction is not matched to a command.
  Completion* completions = static_cast<Completion*>(pair->completion().head());
  memset(completions, 0, sizeof(*completions) * pair->completion().entry_count());
  completions[0].set_command_id(0);
  completions[0].set_phase(1);
  completions[0].set_sq_head(0);

  Completion* completion = nullptr;
  IoCommand* io_cmd = &data_cmd;
  EXPECT_EQ(pair->CheckForNewCompletion(&completion, &io_cmd), ZX_ERR_BAD_STATE);
  EXPECT_EQ(io_cmd, nullptr);
  EXPECT_EQ(pair->in_flight_count(), 0u);
}

TEST_F(QueuePairTest, TestCancelAllWithoutCallback) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 1, 100, caps_, mmio_, /*prealloc_prp=*/true);
  ASSERT_OK(pair.status_value());

  doorbell_ring_ = [](bool, size_t, uint16_t) {};

  IoCommand cmd;
  Submission s(0);
  ASSERT_OK(pair->Submit(s, std::nullopt, 0, 0, &cmd));
  EXPECT_EQ(pair->in_flight_count(), 1u);

  pair->CancelAll(nullptr);
  EXPECT_EQ(pair->in_flight_count(), 0u);
  EXPECT_FALSE(txn(pair.value().get(), 0).active);
}

// Not using QueuePair::PreparePrpList() for now. See QueuePair::kMaxTransferPages.
TEST_F(QueuePairTest, DISABLED_TestSubmitWithDataManyPages) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 0, 100, caps_, mmio_, /*prealloc_prp=*/false);
  ASSERT_OK(pair.status_value());

  doorbell_ring_ = [](bool is_completion, size_t queue_id, uint16_t new_value) {
    ASSERT_FALSE(is_completion);
    ASSERT_EQ(0u, queue_id);
    ASSERT_EQ(1u, new_value);
  };
  zx::vmo data_vmo;
  constexpr size_t kNumPages = 4;
  ASSERT_OK(zx::vmo::create(kNumPages * zx_system_get_page_size(), 0, &data_vmo));
  Submission s(0xa9);
  {
    ASSERT_OK(pair->Submit(s, data_vmo.borrow(), 0, kNumPages * zx_system_get_page_size()));
  }

  Submission* submitted = static_cast<Submission*>(pair->submission().head());
  ASSERT_EQ(0u, submitted->data_transfer_mode());
  ASSERT_EQ(0u, submitted->fused());
  ASSERT_EQ(0xa9u, submitted->opcode());
  ASSERT_EQ(uint64_t{FAKE_BTI_PHYS_ADDR}, submitted->data_pointer[0]);
  ASSERT_EQ(uint64_t{FAKE_BTI_PHYS_ADDR}, submitted->data_pointer[1]);
  TransactionData& txn_data = txn(pair.value().get(), 0);
  ASSERT_TRUE(txn_data.prp_buffer);
  ASSERT_TRUE(txn_data.active);
  uint64_t* prps = static_cast<uint64_t*>(txn_data.prp_buffer->virt());
  for (size_t i = 0; i < kNumPages - 1; i++) {
    ASSERT_EQ(uint64_t{FAKE_BTI_PHYS_ADDR}, prps[i]);
  }
  ASSERT_EQ(0u, prps[kNumPages - 1]);
}

// Not using QueuePair::PreparePrpList() for now. See QueuePair::kMaxTransferPages.
TEST_F(QueuePairTest, DISABLED_TestSubmitWithMultiPagePrp) {
  auto pair = QueuePair::Create(fake_bti_.borrow(), 0, 100, caps_, mmio_, /*prealloc_prp=*/false);
  ASSERT_OK(pair.status_value());

  doorbell_ring_ = [](bool is_completion, size_t queue_id, uint16_t new_value) {
    ASSERT_FALSE(is_completion);
    ASSERT_EQ(0u, queue_id);
    ASSERT_EQ(1u, new_value);
  };
  zx::vmo data_vmo;
  const size_t addr_per_page = zx_system_get_page_size() / sizeof(uint64_t);
  const size_t kNumAddresses = addr_per_page + 10;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size() * kNumAddresses, 0, &data_vmo));
  Submission s(0xa9);
  {
    ASSERT_OK(pair->Submit(s, data_vmo.borrow(), 0, zx_system_get_page_size() * kNumAddresses));
  }

  Submission* submitted = static_cast<Submission*>(pair->submission().head());
  ASSERT_EQ(0u, submitted->data_transfer_mode());
  ASSERT_EQ(0u, submitted->fused());
  ASSERT_EQ(0xa9u, submitted->opcode());
  ASSERT_EQ(uint64_t{FAKE_BTI_PHYS_ADDR}, submitted->data_pointer[0]);
  ASSERT_EQ(uint64_t{FAKE_BTI_PHYS_ADDR}, submitted->data_pointer[1]);
  TransactionData& txn_data = txn(pair.value().get(), 0);
  ASSERT_TRUE(txn_data.prp_buffer);
  ASSERT_TRUE(txn_data.active);
  uint64_t* prps = static_cast<uint64_t*>(txn_data.prp_buffer->virt());
  for (size_t i = 0; i < kNumAddresses; i++) {
    ZX_ASSERT_MSG(FAKE_BTI_PHYS_ADDR == prps[i], "PRP %zu had wrong value", i);
  }
  ASSERT_EQ(0u, prps[kNumAddresses]);
}
}  // namespace nvme
