// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fit/defer.h>

#include <array>
#include <atomic>
#include <cstdint>
#include <cstring>
#include <memory>
#include <vector>

#include "src/devices/block/drivers/ufs/registers.h"
#include "src/devices/block/drivers/ufs/transfer_request_descriptor.h"
#include "src/devices/block/drivers/ufs/upiu/descriptors.h"
#include "src/devices/block/drivers/ufs/upiu/upiu_transactions.h"
#include "unit-lib.h"
#include "zircon/errors.h"

namespace ufs {
using namespace ufs_mock_device;

namespace {

constexpr zx::duration kWaitLimit = zx::sec(30);

// Returning ZX_ERR_TIMED_OUT makes the mock transfer processor leave UTRLDBR[slot] set.
void HookRead10ToStall(UfsMockDevice& mock_device) {
  mock_device.GetScsiCommandProcessor().SetHook(
      scsi::Opcode::READ_10,
      [](UfsMockDevice& mock_device, CommandUpiuData& command_upiu, ResponseUpiuData& response_upiu,
         cpp20::span<PhysicalRegionDescriptionTableEntry>& prdt_upius) {
        return zx::error(ZX_ERR_TIMED_OUT);
      });
}

}  // namespace

using TimeoutTest = UfsTest;

TEST_F(TimeoutTest, GetEarliestTimeoutDeadline) {
  constexpr uint8_t kTestLun = 0;
  const uint8_t kMaxSlotCount =
      dut_->GetTransferRequestProcessor().GetSlotCount() - kAdminCommandSlotCount;

  // Disable IoLoop completion
  dut_->GetTransferRequestProcessor().DisableCompletion();

  // If there is no in-flight I/O, ZX_TIME_INFINITE should be returned.
  {
    zx_time_t deadline = dut_->GetTransferRequestProcessor().GetEarliestTimeoutDeadline();
    ASSERT_EQ(ZX_TIME_INFINITE, deadline);
  }

  // If there are in-flight I/Os, it should return the earliest timeout deadline.
  {
    uint8_t cdb_buffer[6] = {};
    auto cdb = reinterpret_cast<scsi::TestUnitReadyCDB*>(cdb_buffer);
    cdb->opcode = scsi::Opcode::TEST_UNIT_READY;

    ScsiCommandUpiu upiu(cdb_buffer, sizeof(*cdb), DataDirection::kNone);
    for (uint8_t slot_num = 0; slot_num < kMaxSlotCount; ++slot_num) {
      zx::result<uint8_t> slot = dut_->GetTransferRequestProcessor().ReserveSlot();
      ASSERT_OK(slot);
      dut_->GetTransferRequestProcessor().SendIoScsiCmd(
          upiu, kTestLun, slot.value(), zx::unowned_vmo(), 0, 0, [](zx_status_t status) {});
    }

    // Request in slot 0 is the earliest issued request.
    zx_time_t slot_0_deadline;
    {
      std::lock_guard<std::mutex> lock(dut_->GetTransferRequestProcessor().GetSlotLock());
      slot_0_deadline =
          dut_->GetTransferRequestProcessor().GetRequestListLocked().GetSlot(0).deadline;
      for (uint8_t slot_num = 0; slot_num < kMaxSlotCount; ++slot_num) {
        EXPECT_LE(
            slot_0_deadline,
            dut_->GetTransferRequestProcessor().GetRequestListLocked().GetSlot(slot_num).deadline);
      }
    }

    // Wait 100 ms for outstanding send requests to complete.
    usleep(100000);

    zx_time_t deadline = dut_->GetTransferRequestProcessor().GetEarliestTimeoutDeadline();
    ASSERT_EQ(slot_0_deadline, deadline);

    dut_->GetTransferRequestProcessor().EnableCompletion();
    ASSERT_EQ(dut_->GetTransferRequestProcessor().ProcessCompletionOfIoRequests(), kMaxSlotCount);
  }
}

TEST_F(TimeoutTest, StalledIoIsNeverAbortedAndStaysPinned) {
  constexpr uint8_t kTestLun = 0;
  constexpr uint8_t target_task_tag = 0;

  auto lun_id = Ufs::TranslateScsiLunToUfsLun(kTestLun);
  ASSERT_OK(lun_id);

  dut_->GetTransferRequestProcessor().SetTimeout(zx::msec(100));

  uint8_t cdb_buffer[16] = {};
  uint8_t cdb_length;
  auto cdb = reinterpret_cast<scsi::Read10CDB*>(cdb_buffer);
  cdb_length = 10;
  cdb->opcode = scsi::Opcode::READ_10;
  cdb->logical_block_address = 0;
  cdb->transfer_length = 0;
  cdb->set_force_unit_access(false);
  ZX_ASSERT(cdb_length <= sizeof(cdb_buffer));

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(ufs_mock_device::kMockBlockSize, 0, &vmo));
  std::atomic<uint32_t> utrlclr_writes{0};
  sync_completion_t complete;
  zx_status_t completion_status = ZX_OK;
  // Stop the driver before any stack variables captured by completion callbacks or MMIO hooks go
  // out of scope: Ufs::Stop() aborts in-flight stalled slots via their completion callbacks.
  auto cleanup = fit::defer([this]() {
    if (!driver_stopped_) {
      driver_stopped_ = true;
      EXPECT_OK(driver_test().StopDriver());
    }
    mock_device_.GetScsiCommandProcessor().Reset();
    mock_device_.GetRegisterMmioProcessor().Reset();
  });

  HookRead10ToStall(mock_device_);
  mock_device_.GetRegisterMmioProcessor().SetHook(
      RegisterMap::kUTRLCLR, [&](UfsMockDevice& mock_device, uint32_t value) {
        utrlclr_writes.fetch_add(1);
        RegisterMmioProcessor::DefaultUTRLCLRHandler(mock_device, value);
      });

  ScsiCommandUpiu upiu(cdb_buffer, cdb_length, DataDirection::kDeviceToHost,
                       ufs_mock_device::kMockBlockSize);
  zx::result<uint8_t> slot = dut_->GetTransferRequestProcessor().ReserveSlot();
  ASSERT_OK(slot);
  ASSERT_EQ(slot.value(), target_task_tag);
  dut_->GetTransferRequestProcessor().SendIoScsiCmd(upiu, kTestLun, slot.value(), vmo.borrow(), 0,
                                                    ufs_mock_device::kMockBlockSize,
                                                    [&](zx_status_t status) {
                                                      completion_status = status;
                                                      sync_completion_signal(&complete);
                                                    });

  auto wait_for = [&]() -> bool {
    dut_->ProcessIoCompletions();
    return dut_->GetTransferRequestProcessor().GetTimeoutLogCountForTesting() >= 1;
  };
  fbl::String timeout_message = "Timeout waiting for the overdue I/O slot to be logged";
  ASSERT_OK(dut_->WaitWithTimeout(wait_for, kWaitLimit, timeout_message, zx::msec(100)));

  // Disable background IoLoop completion polling so the manual passes below advance the deadline
  // deterministically once per timeout window.
  dut_->GetTransferRequestProcessor().DisableCompletion();
  const uint64_t initial_logs = dut_->GetTransferRequestProcessor().GetTimeoutLogCountForTesting();
  dut_->GetTransferRequestProcessor().EnableCompletion();

  // Poll across several more command-timeout windows to verify that elapsed time logs on each
  // expired deadline while never aborting the I/O slot or unpinning its DMA buffer.
  dut_->GetTransferRequestProcessor().DisableCompletion();
  for (int i = 0; i < 4; ++i) {
    zx::nanosleep(zx::deadline_after(zx::msec(110)));
    dut_->GetTransferRequestProcessor().EnableCompletion();
    EXPECT_EQ(dut_->GetTransferRequestProcessor().ProcessCompletionOfIoRequests(), 0u);
    dut_->GetTransferRequestProcessor().DisableCompletion();
  }
  dut_->GetTransferRequestProcessor().EnableCompletion();
  EXPECT_GE(dut_->GetTransferRequestProcessor().GetTimeoutLogCountForTesting(), initial_logs + 4);

  zx_time_t next_deadline = ZX_TIME_INFINITE;
  {
    std::lock_guard<std::mutex> lock(dut_->GetTransferRequestProcessor().GetSlotLock());
    const RequestSlot& req_slot =
        dut_->GetTransferRequestProcessor().GetRequestListLocked().GetSlot(target_task_tag);
    ASSERT_EQ(req_slot.state, SlotState::kScheduled);
    ASSERT_TRUE(req_slot.pmt.is_valid());
    ASSERT_TRUE(req_slot.completion_cb);
    next_deadline = req_slot.deadline;
  }
  EXPECT_FALSE(sync_completion_signaled(&complete));
  EXPECT_EQ(utrlclr_writes.load(), 0u);
  ASSERT_EQ(dut_->GetTransferRequestProcessor().GetEarliestTimeoutDeadline(), next_deadline);
  ASSERT_NE(
      UtrListDoorBellReg::Get().ReadFrom(&dut_->GetMmio()).door_bell() & (1u << target_task_tag),
      0u);
  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetQuarantinedPageCountForTesting(), 0u);

  // Driver teardown disables HCE to stop DMA before unpinning and completing the stalled slot with
  // ZX_ERR_IO_NOT_PRESENT, so BlockDevice::ShutdownAsync() does not hang on a stranded bio.
  driver_stopped_ = true;
  ASSERT_OK(driver_test().StopDriver());
  ASSERT_OK(sync_completion_wait(&complete, ZX_TIME_INFINITE));
  EXPECT_EQ(completion_status, ZX_ERR_IO_NOT_PRESENT);
  EXPECT_EQ(utrlclr_writes.load(), 0u);
}

TEST_F(TimeoutTest, AllIoSlotsStalledStayScheduled) {
  constexpr uint8_t kTestLun = 0;
  const uint8_t kMaxSlotCount =
      dut_->GetTransferRequestProcessor().GetSlotCount() - kAdminCommandSlotCount;

  auto lun_id = Ufs::TranslateScsiLunToUfsLun(kTestLun);
  ASSERT_OK(lun_id);

  dut_->GetTransferRequestProcessor().DisableCompletion();
  auto enable_completion =
      fit::defer([this]() { dut_->GetTransferRequestProcessor().EnableCompletion(); });

  dut_->GetTransferRequestProcessor().SetTimeout(zx::msec(100));

  uint8_t cdb_buffer[16] = {};
  uint8_t cdb_length;
  auto cdb = reinterpret_cast<scsi::Read10CDB*>(cdb_buffer);
  cdb_length = 10;
  cdb->opcode = scsi::Opcode::READ_10;
  cdb->logical_block_address = 0;
  cdb->transfer_length = 0;
  cdb->set_force_unit_access(false);
  ZX_ASSERT(cdb_length <= sizeof(cdb_buffer));

  HookRead10ToStall(mock_device_);

  auto vmos = std::make_unique<zx::vmo[]>(kMaxSlotCount);

  for (uint8_t slot_num = 0; slot_num < kMaxSlotCount; ++slot_num) {
    ASSERT_OK(zx::vmo::create(ufs_mock_device::kMockBlockSize, 0, &vmos[slot_num]));
    ScsiCommandUpiu upiu(cdb_buffer, cdb_length, DataDirection::kDeviceToHost,
                         ufs_mock_device::kMockBlockSize);
    zx::result<uint8_t> slot = dut_->GetTransferRequestProcessor().ReserveSlot();
    ASSERT_OK(slot);
    ASSERT_EQ(slot.value(), slot_num);
    dut_->GetTransferRequestProcessor().SendIoScsiCmd(
        upiu, kTestLun, slot.value(), vmos[slot_num].borrow(), 0, ufs_mock_device::kMockBlockSize,
        [](zx_status_t status) {});
  }

  dut_->GetTransferRequestProcessor().EnableCompletion();
  enable_completion.cancel();

  auto wait_for = [&]() -> bool {
    dut_->ProcessIoCompletions();
    return dut_->GetTransferRequestProcessor().GetTimeoutLogCountForTesting() >= kMaxSlotCount;
  };
  fbl::String timeout_message = "Timeout waiting for overdue I/O slots to be logged";
  ASSERT_OK(dut_->WaitWithTimeout(wait_for, kWaitLimit, timeout_message, zx::msec(100)));

  zx_time_t earliest_deadline = ZX_TIME_INFINITE;
  {
    std::lock_guard<std::mutex> lock(dut_->GetTransferRequestProcessor().GetSlotLock());
    for (uint8_t slot_num = 0; slot_num < kMaxSlotCount; ++slot_num) {
      const RequestSlot& req_slot =
          dut_->GetTransferRequestProcessor().GetRequestListLocked().GetSlot(slot_num);
      EXPECT_EQ(req_slot.state, SlotState::kScheduled);
      EXPECT_TRUE(req_slot.pmt.is_valid());
      earliest_deadline = std::min(earliest_deadline, req_slot.deadline);
    }
  }
  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetEarliestTimeoutDeadline(), earliest_deadline);
  driver_stopped_ = true;
  ASSERT_OK(driver_test().StopDriver());
  mock_device_.GetScsiCommandProcessor().Reset();
}

TEST_F(TimeoutTest, PartialIoStallsDoNotBlockOtherCompletions) {
  constexpr uint8_t kTestLun = 0;
  const uint8_t kMaxSlotCount =
      dut_->GetTransferRequestProcessor().GetSlotCount() - kAdminCommandSlotCount;
  const uint8_t kStalledCount = kMaxSlotCount / 2;

  auto lun_id = Ufs::TranslateScsiLunToUfsLun(kTestLun);
  ASSERT_OK(lun_id);

  dut_->GetTransferRequestProcessor().DisableCompletion();
  auto enable_completion =
      fit::defer([this]() { dut_->GetTransferRequestProcessor().EnableCompletion(); });

  dut_->GetTransferRequestProcessor().SetTimeout(zx::msec(100));

  uint8_t cdb_buffer[16] = {};
  uint8_t cdb_length;
  auto cdb = reinterpret_cast<scsi::Read10CDB*>(cdb_buffer);
  cdb_length = 10;
  cdb->opcode = scsi::Opcode::READ_10;
  cdb->logical_block_address = 0;
  cdb->transfer_length = 0;
  cdb->set_force_unit_access(false);
  ZX_ASSERT(cdb_length <= sizeof(cdb_buffer));

  HookRead10ToStall(mock_device_);

  auto vmos = std::make_unique<zx::vmo[]>(kMaxSlotCount);

  cdb->opcode = scsi::Opcode::READ_10;
  for (uint8_t slot_num = 0; slot_num < kStalledCount; ++slot_num) {
    ASSERT_OK(zx::vmo::create(ufs_mock_device::kMockBlockSize, 0, &vmos[slot_num]));
    ScsiCommandUpiu upiu(cdb_buffer, cdb_length, DataDirection::kDeviceToHost,
                         ufs_mock_device::kMockBlockSize);
    zx::result<uint8_t> slot = dut_->GetTransferRequestProcessor().ReserveSlot();
    ASSERT_OK(slot);
    ASSERT_EQ(slot.value(), slot_num);
    dut_->GetTransferRequestProcessor().SendIoScsiCmd(
        upiu, kTestLun, slot.value(), vmos[slot_num].borrow(), 0, ufs_mock_device::kMockBlockSize,
        [](zx_status_t status) {});
  }

  cdb->opcode = scsi::Opcode::WRITE_10;
  cdb->transfer_length = htobe16(1);
  for (uint8_t slot_num = kStalledCount; slot_num < kMaxSlotCount; ++slot_num) {
    ASSERT_OK(zx::vmo::create(ufs_mock_device::kMockBlockSize, 0, &vmos[slot_num]));
    ScsiCommandUpiu upiu(cdb_buffer, cdb_length, DataDirection::kHostToDevice,
                         ufs_mock_device::kMockBlockSize);
    zx::result<uint8_t> slot = dut_->GetTransferRequestProcessor().ReserveSlot();
    ASSERT_OK(slot);
    ASSERT_EQ(slot.value(), slot_num);
    dut_->GetTransferRequestProcessor().SendIoScsiCmd(
        upiu, kTestLun, slot.value(), vmos[slot_num].borrow(), 0, ufs_mock_device::kMockBlockSize,
        [](zx_status_t status) {});
  }

  dut_->GetTransferRequestProcessor().EnableCompletion();
  enable_completion.cancel();

  auto wait_for = [&]() -> bool {
    dut_->ProcessIoCompletions();
    if (dut_->GetTransferRequestProcessor().GetTimeoutLogCountForTesting() < kStalledCount) {
      return false;
    }
    std::lock_guard<std::mutex> lock(dut_->GetTransferRequestProcessor().GetSlotLock());
    for (uint8_t slot_num = 0; slot_num < kStalledCount; ++slot_num) {
      if (dut_->GetTransferRequestProcessor().GetRequestListLocked().GetSlot(slot_num).state !=
          SlotState::kScheduled) {
        return false;
      }
    }
    for (uint8_t slot_num = kStalledCount; slot_num < kMaxSlotCount; ++slot_num) {
      if (dut_->GetTransferRequestProcessor().GetRequestListLocked().GetSlot(slot_num).state !=
          SlotState::kFree) {
        return false;
      }
    }
    return true;
  };
  fbl::String timeout_message = "Timeout waiting for overdue I/O logs or completions";
  ASSERT_OK(dut_->WaitWithTimeout(wait_for, kWaitLimit, timeout_message, zx::msec(100)));

  {
    std::lock_guard<std::mutex> lock(dut_->GetTransferRequestProcessor().GetSlotLock());
    for (uint8_t slot_num = 0; slot_num < kStalledCount; ++slot_num) {
      const RequestSlot& req_slot =
          dut_->GetTransferRequestProcessor().GetRequestListLocked().GetSlot(slot_num);
      EXPECT_EQ(req_slot.state, SlotState::kScheduled);
    }

    for (uint8_t slot_num = kStalledCount; slot_num < kMaxSlotCount; ++slot_num) {
      EXPECT_EQ(dut_->GetTransferRequestProcessor().GetRequestListLocked().GetSlot(slot_num).state,
                SlotState::kFree);
      EXPECT_EQ(dut_->GetTransferRequestProcessor().GetRequestListLocked().GetSlot(slot_num).result,
                ZX_OK);
    }
  }
  driver_stopped_ = true;
  ASSERT_OK(driver_test().StopDriver());
  mock_device_.GetScsiCommandProcessor().Reset();
}

TEST_F(TimeoutTest, StalledIoCompletesOkOnLateDoorbellClearAndReadsUecOncePerPass) {
  constexpr uint8_t kTestLun = 0;
  dut_->GetTransferRequestProcessor().DisableCompletion();
  dut_->GetTransferRequestProcessor().SetTimeout(zx::msec(20));

  std::vector<uint8_t> expected(kMockBlockSize, 0xa5);
  ASSERT_OK(mock_device_.BufferWrite(kTestLun, expected.data(), /*block_count=*/1,
                                     /*block_offset=*/0));

  std::atomic<uint32_t> utrlclr_writes{0};
  std::atomic<uint32_t> uecpa_reads{0};
  std::atomic<uint32_t> uecdl_reads{0};
  std::atomic<uint32_t> uecn_reads{0};
  std::atomic<uint32_t> uect_reads{0};
  std::atomic<uint32_t> uecdme_reads{0};
  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(kMockBlockSize, 0, &vmo));
  sync_completion_t complete;
  zx_status_t completion_status = ZX_ERR_INTERNAL;
  auto cleanup = fit::defer([this]() {
    dut_->GetTransferRequestProcessor().EnableCompletion();
    if (!driver_stopped_) {
      driver_stopped_ = true;
      EXPECT_OK(driver_test().StopDriver());
    }
    mock_device_.GetScsiCommandProcessor().Reset();
    mock_device_.GetRegisterMmioProcessor().Reset();
  });

  mock_device_.GetRegisterMmioProcessor().SetHook(
      RegisterMap::kUTRLCLR, [&](UfsMockDevice& mock_device, uint32_t value) {
        utrlclr_writes.fetch_add(1);
        RegisterMmioProcessor::DefaultUTRLCLRHandler(mock_device, value);
      });
  auto install_read_to_clear = [&](RegisterMap reg, uint32_t initial_val,
                                   std::atomic<uint32_t>& counter) {
    mock_device_.GetRegisters()->Write<uint32_t>(initial_val, reg);
    mock_device_.GetRegisterMmioProcessor().SetReadHook(reg, [&, reg](UfsMockDevice& mock_device) {
      counter.fetch_add(1);
      const uint32_t val = mock_device.GetRegisters()->Read<uint32_t>(reg);
      mock_device.GetRegisters()->Write<uint32_t>(0, reg);
      return val;
    });
  };
  install_read_to_clear(RegisterMap::kUECPA, 0x80000001u, uecpa_reads);
  install_read_to_clear(RegisterMap::kUECDL, 0x80000002u, uecdl_reads);
  install_read_to_clear(RegisterMap::kUECN, 0x80000003u, uecn_reads);
  install_read_to_clear(RegisterMap::kUECT, 0x80000004u, uect_reads);
  install_read_to_clear(RegisterMap::kUECDME, 0x80000005u, uecdme_reads);

  HookRead10ToStall(mock_device_);

  uint8_t cdb_buffer[16] = {};
  auto* cdb = reinterpret_cast<scsi::Read10CDB*>(cdb_buffer);
  cdb->opcode = scsi::Opcode::READ_10;
  cdb->logical_block_address = htobe32(0);
  cdb->transfer_length = htobe16(1);

  ScsiCommandUpiu upiu(cdb_buffer, 10, DataDirection::kDeviceToHost, kMockBlockSize);
  zx::result<uint8_t> slot = dut_->GetTransferRequestProcessor().ReserveSlot();
  ASSERT_OK(slot);
  dut_->GetTransferRequestProcessor().SendIoScsiCmd(upiu, kTestLun, slot.value(), vmo.borrow(), 0,
                                                    kMockBlockSize, [&](zx_status_t status) {
                                                      completion_status = status;
                                                      sync_completion_signal(&complete);
                                                    });

  zx::nanosleep(zx::deadline_after(zx::msec(30)));
  // Run two completion passes across two deadline windows: each pass logs once and captures the
  // read-to-clear UEC* registers once.
  dut_->GetTransferRequestProcessor().EnableCompletion();
  EXPECT_EQ(dut_->GetTransferRequestProcessor().ProcessCompletionOfIoRequests(), 0u);
  dut_->GetTransferRequestProcessor().DisableCompletion();
  zx::nanosleep(zx::deadline_after(zx::msec(30)));
  dut_->GetTransferRequestProcessor().EnableCompletion();
  EXPECT_EQ(dut_->GetTransferRequestProcessor().ProcessCompletionOfIoRequests(), 0u);
  dut_->GetTransferRequestProcessor().DisableCompletion();

  EXPECT_EQ(utrlclr_writes.load(), 0u);
  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetTimeoutLogCountForTesting(), 2u);
  EXPECT_EQ(uecpa_reads.load(), 2u);
  EXPECT_EQ(uecdl_reads.load(), 2u);
  EXPECT_EQ(uecn_reads.load(), 2u);
  EXPECT_EQ(uect_reads.load(), 2u);
  EXPECT_EQ(uecdme_reads.load(), 2u);
  EXPECT_FALSE(sync_completion_signaled(&complete));
  zx_time_t next_deadline = ZX_TIME_INFINITE;
  {
    std::lock_guard<std::mutex> lock(dut_->GetTransferRequestProcessor().GetSlotLock());
    const RequestSlot& req_slot =
        dut_->GetTransferRequestProcessor().GetRequestListLocked().GetSlot(slot.value());
    EXPECT_EQ(req_slot.state, SlotState::kScheduled);
    EXPECT_TRUE(req_slot.pmt.is_valid());
    next_deadline = req_slot.deadline;
  }
  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetEarliestTimeoutDeadline(), next_deadline);
  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetIoSlotState().earliest_timeout_deadline,
            next_deadline);

  // Simulate late hardware completion DMAing into the still-pinned VMO and clearing UTRLDBR.
  mock_device_.GetScsiCommandProcessor().Reset();
  {
    std::lock_guard<std::mutex> lock(dut_->GetTransferRequestProcessor().GetSlotLock());
    auto* desc = dut_->GetTransferRequestProcessor()
                     .GetRequestListLocked()
                     .GetRequestDescriptor<TransferRequestDescriptor>(slot.value());
    desc->set_interrupt(false);
    ASSERT_OK(mock_device_.GetTransferRequestProcessor().HandleTransferRequest(*desc));
  }
  const uint32_t doorbell =
      UtrListDoorBellReg::Get().ReadFrom(mock_device_.GetRegisters()).door_bell() &
      ~(1u << slot.value());
  UtrListDoorBellReg::Get().FromValue(doorbell).WriteTo(mock_device_.GetRegisters());

  dut_->GetTransferRequestProcessor().EnableCompletion();
  EXPECT_EQ(dut_->GetTransferRequestProcessor().ProcessCompletionOfIoRequests(), 1u);
  ASSERT_OK(sync_completion_wait(&complete, ZX_TIME_INFINITE));
  EXPECT_EQ(completion_status, ZX_OK);

  std::vector<uint8_t> actual(kMockBlockSize, 0);
  ASSERT_OK(vmo.read(actual.data(), 0, actual.size()));
  EXPECT_EQ(actual, expected);
  {
    std::lock_guard<std::mutex> lock(dut_->GetTransferRequestProcessor().GetSlotLock());
    const RequestSlot& req_slot =
        dut_->GetTransferRequestProcessor().GetRequestListLocked().GetSlot(slot.value());
    EXPECT_EQ(req_slot.state, SlotState::kFree);
    EXPECT_FALSE(req_slot.pmt.is_valid());
  }
}

TEST_F(TimeoutTest, SuspendDrainTimesOutWithInflightIoAndForcedPowerDownAbortsCleanly) {
  constexpr uint8_t kTestLun = 0;
  dut_->GetTransferRequestProcessor().SetTimeout(zx::msec(15));
  dut_->GetDeviceManager().SetSuspendIoDrainTimeoutForTesting(zx::msec(30));

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(kMockBlockSize, 0, &vmo));
  sync_completion_t complete;
  zx_status_t completion_status = ZX_OK;
  auto cleanup = fit::defer([this]() {
    if (!driver_stopped_) {
      driver_stopped_ = true;
      EXPECT_OK(driver_test().StopDriver());
    }
    mock_device_.GetScsiCommandProcessor().Reset();
  });

  HookRead10ToStall(mock_device_);

  uint8_t cdb_buffer[16] = {};
  auto* cdb = reinterpret_cast<scsi::Read10CDB*>(cdb_buffer);
  cdb->opcode = scsi::Opcode::READ_10;
  cdb->transfer_length = htobe16(1);

  ScsiCommandUpiu upiu(cdb_buffer, 10, DataDirection::kDeviceToHost, kMockBlockSize);
  zx::result<uint8_t> slot = dut_->GetTransferRequestProcessor().ReserveSlot();
  ASSERT_OK(slot);
  dut_->GetTransferRequestProcessor().SendIoScsiCmd(upiu, kTestLun, slot.value(), vmo.borrow(), 0,
                                                    kMockBlockSize, [&](zx_status_t status) {
                                                      completion_status = status;
                                                      sync_completion_signal(&complete);
                                                    });

  zx::nanosleep(zx::deadline_after(zx::msec(25)));
  EXPECT_EQ(dut_->GetTransferRequestProcessor().ProcessCompletionOfIoRequests(), 0u);
  EXPECT_FALSE(sync_completion_signaled(&complete));

  // 1. SuspendPower() and BlockSlotAllocation() must time out cleanly while the I/O slot remains
  //    scheduled, leaving IsResumed() true and new I/O slots reservable.
  zx::result<> suspend_res = dut_->GetDeviceManager().SuspendPower();
  ASSERT_TRUE(suspend_res.is_error());
  EXPECT_EQ(suspend_res.status_value(), ZX_ERR_TIMED_OUT);
  EXPECT_TRUE(dut_->IsResumed());
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
  EXPECT_FALSE(sync_completion_signaled(&complete));

  zx::result<> drain_res = dut_->GetTransferRequestProcessor().BlockSlotAllocation(zx::msec(30));
  ASSERT_TRUE(drain_res.is_error());
  EXPECT_EQ(drain_res.status_value(), ZX_ERR_TIMED_OUT);
  dut_->GetTransferRequestProcessor().UnblockSlotAllocation();

  zx::result<uint8_t> second_slot = dut_->GetTransferRequestProcessor().ReserveSlot();
  ASSERT_OK(second_slot);
  ASSERT_OK(ClearSlot(second_slot.value()));

  // 2. Forced StopHostControllerForPowerDown() disables HCE (stopping DMA) and aborts the live
  //    in-flight slot with ZX_ERR_CANCELED (so Ufs::ProcessIoSubmissions re-queues block requests
  //    for replay after resume) without stranding the bio or leaking the PMT.
  ASSERT_OK(dut_->StopHostControllerForPowerDown());
  ASSERT_OK(sync_completion_wait(&complete, ZX_TIME_INFINITE));
  EXPECT_EQ(completion_status, ZX_ERR_CANCELED);
  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetInflightIoCount(), 0u);
  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetQuarantinedPmtCountForTesting(), 0u);
  {
    std::lock_guard<std::mutex> lock(dut_->GetTransferRequestProcessor().GetSlotLock());
    const RequestSlot& req_slot =
        dut_->GetTransferRequestProcessor().GetRequestListLocked().GetSlot(slot.value());
    EXPECT_EQ(req_slot.state, SlotState::kFree);
    EXPECT_FALSE(req_slot.pmt.is_valid());
  }
}

TEST_F(TimeoutTest, ForcedPowerDownQuarantinesPmtWhenDisableHostControllerFails) {
  constexpr uint8_t kTestLun = 0;
  constexpr uint32_t kPinnedPages = 2;
  const uint32_t kTransferBytes = kPinnedPages * zx_system_get_page_size();
  const uint16_t kTransferBlocks = static_cast<uint16_t>(kTransferBytes / kMockBlockSize);
  dut_->GetTransferRequestProcessor().SetTimeout(zx::msec(15));

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(kTransferBytes, 0, &vmo));
  sync_completion_t complete;
  zx_status_t completion_status = ZX_OK;
  auto cleanup = fit::defer([this]() {
    if (!driver_stopped_) {
      driver_stopped_ = true;
      EXPECT_OK(driver_test().StopDriver());
    }
    mock_device_.GetRegisterMmioProcessor().Reset();
    mock_device_.GetScsiCommandProcessor().Reset();
  });

  HookRead10ToStall(mock_device_);
  // Make HCE = 0 fail to clear so DMA cannot be confirmed stopped.
  mock_device_.GetRegisterMmioProcessor().SetHook(RegisterMap::kHCE,
                                                  RegisterMmioProcessor::NoOpHandler);

  uint8_t cdb_buffer[16] = {};
  auto* cdb = reinterpret_cast<scsi::Read10CDB*>(cdb_buffer);
  cdb->opcode = scsi::Opcode::READ_10;
  cdb->transfer_length = htobe16(kTransferBlocks);

  ScsiCommandUpiu upiu(cdb_buffer, 10, DataDirection::kDeviceToHost, kTransferBytes);
  zx::result<uint8_t> slot = dut_->GetTransferRequestProcessor().ReserveSlot();
  ASSERT_OK(slot);
  dut_->GetTransferRequestProcessor().SendIoScsiCmd(upiu, kTestLun, slot.value(), vmo.borrow(), 0,
                                                    kTransferBytes, [&](zx_status_t status) {
                                                      completion_status = status;
                                                      sync_completion_signal(&complete);
                                                    });

  EXPECT_NE(dut_->StopHostControllerForPowerDown(), ZX_OK);
  ASSERT_OK(sync_completion_wait(&complete, ZX_TIME_INFINITE));
  EXPECT_EQ(completion_status, ZX_ERR_CANCELED);
  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetInflightIoCount(), 0u);
  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetQuarantinedPmtCountForTesting(), 1u);
  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetQuarantinedPageCountForTesting(), kPinnedPages);
}

TEST_F(TimeoutTest, MultipleOverdueSlotsLogEachPassAndReadUecOncePerPass) {
  constexpr uint8_t kTestLun = 0;
  constexpr uint8_t kStalledCount = 6;
  std::atomic<uint32_t> utrlclr_writes{0};
  std::atomic<uint32_t> uecpa_reads{0};
  std::array<zx::vmo, kStalledCount> vmos;
  std::array<uint8_t, kStalledCount> slots{};
  std::atomic<uint32_t> completions{0};
  std::atomic<zx_status_t> last_status{ZX_OK};
  dut_->GetTransferRequestProcessor().DisableCompletion();
  auto cleanup = fit::defer([this]() {
    dut_->GetTransferRequestProcessor().EnableCompletion();
    if (!driver_stopped_) {
      driver_stopped_ = true;
      EXPECT_OK(driver_test().StopDriver());
    }
    mock_device_.GetScsiCommandProcessor().Reset();
    mock_device_.GetRegisterMmioProcessor().Reset();
  });

  dut_->GetTransferRequestProcessor().SetTimeout(zx::msec(10));

  HookRead10ToStall(mock_device_);
  mock_device_.GetRegisterMmioProcessor().SetHook(
      RegisterMap::kUTRLCLR, [&](UfsMockDevice& mock_device, uint32_t value) {
        utrlclr_writes.fetch_add(1);
        RegisterMmioProcessor::DefaultUTRLCLRHandler(mock_device, value);
      });
  mock_device_.GetRegisters()->Write<uint32_t>(0x80000001u, RegisterMap::kUECPA);
  mock_device_.GetRegisterMmioProcessor().SetReadHook(
      RegisterMap::kUECPA, [&](UfsMockDevice& mock_device) {
        uecpa_reads.fetch_add(1);
        const uint32_t val = mock_device.GetRegisters()->Read<uint32_t>(RegisterMap::kUECPA);
        mock_device.GetRegisters()->Write<uint32_t>(0, RegisterMap::kUECPA);
        return val;
      });

  uint8_t cdb_buffer[16] = {};
  auto* cdb = reinterpret_cast<scsi::Read10CDB*>(cdb_buffer);
  cdb->opcode = scsi::Opcode::READ_10;
  cdb->transfer_length = htobe16(1);
  ScsiCommandUpiu upiu(cdb_buffer, 10, DataDirection::kDeviceToHost, kMockBlockSize);

  for (uint8_t i = 0; i < kStalledCount; ++i) {
    ASSERT_OK(zx::vmo::create(kMockBlockSize, 0, &vmos[i]));
    zx::result<uint8_t> slot = dut_->GetTransferRequestProcessor().ReserveSlot();
    ASSERT_OK(slot);
    slots[i] = slot.value();
    dut_->GetTransferRequestProcessor().SendIoScsiCmd(
        upiu, kTestLun, slot.value(), vmos[i].borrow(), 0, kMockBlockSize, [&](zx_status_t status) {
          last_status.store(status);
          completions.fetch_add(1);
        });
  }

  // First pass past the command timeout logs each overdue slot once while reading read-to-clear
  // UECPA only once for the entire pass.
  zx::nanosleep(zx::deadline_after(zx::msec(20)));
  dut_->GetTransferRequestProcessor().EnableCompletion();
  EXPECT_EQ(dut_->GetTransferRequestProcessor().ProcessCompletionOfIoRequests(), 0u);
  dut_->GetTransferRequestProcessor().DisableCompletion();
  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetTimeoutLogCountForTesting(), kStalledCount);
  EXPECT_EQ(uecpa_reads.load(), 1u);

  // Second pass after the re-armed deadline logs each overdue slot again and reads UECPA once more,
  // without aborting anything.
  zx::nanosleep(zx::deadline_after(zx::msec(20)));
  dut_->GetTransferRequestProcessor().EnableCompletion();
  EXPECT_EQ(dut_->GetTransferRequestProcessor().ProcessCompletionOfIoRequests(), 0u);
  dut_->GetTransferRequestProcessor().DisableCompletion();
  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetTimeoutLogCountForTesting(), 2u * kStalledCount);
  EXPECT_EQ(uecpa_reads.load(), 2u);
  EXPECT_EQ(completions.load(), 0u);
  EXPECT_EQ(utrlclr_writes.load(), 0u);
  {
    std::lock_guard<std::mutex> lock(dut_->GetTransferRequestProcessor().GetSlotLock());
    for (uint8_t slot_num : slots) {
      const RequestSlot& req_slot =
          dut_->GetTransferRequestProcessor().GetRequestListLocked().GetSlot(slot_num);
      EXPECT_EQ(req_slot.state, SlotState::kScheduled);
      EXPECT_TRUE(req_slot.pmt.is_valid());
    }
  }
  const uint32_t doorbell = UtrListDoorBellReg::Get().ReadFrom(&dut_->GetMmio()).door_bell();
  for (uint8_t slot_num : slots) {
    EXPECT_NE(doorbell & (1u << slot_num), 0u);
  }

  dut_->GetTransferRequestProcessor().EnableCompletion();
  driver_stopped_ = true;
  ASSERT_OK(driver_test().StopDriver());
  EXPECT_EQ(completions.load(), kStalledCount);
  EXPECT_EQ(last_status.load(), ZX_ERR_IO_NOT_PRESENT);
  EXPECT_EQ(utrlclr_writes.load(), 0u);
}

TEST_F(TimeoutTest, AdminCommandTimeoutClearsCompletionCallback) {
  // Emulate a timeout situation for admin command.
  mock_device_.GetScsiCommandProcessor().SetHook(
      scsi::Opcode::TEST_UNIT_READY,
      [](UfsMockDevice& mock_device, CommandUpiuData& command_upiu, ResponseUpiuData& response_upiu,
         cpp20::span<PhysicalRegionDescriptionTableEntry>& prdt_upius) {
        return zx::error(ZX_ERR_TIMED_OUT);
      });

  dut_->GetTransferRequestProcessor().SetTimeout(zx::msec(10));

  uint8_t cdb_buffer[6] = {};
  auto cdb = reinterpret_cast<scsi::TestUnitReadyCDB*>(cdb_buffer);
  cdb->opcode = scsi::Opcode::TEST_UNIT_READY;
  ScsiCommandUpiu upiu(cdb_buffer, sizeof(*cdb), DataDirection::kNone);

  zx::result result = dut_->GetTransferRequestProcessor().SendAdminScsiCmd(upiu, 0);
  EXPECT_TRUE(result.is_error());
  EXPECT_EQ(result.status_value(), ZX_ERR_TIMED_OUT);

  {
    std::lock_guard<std::mutex> lock(dut_->GetTransferRequestProcessor().GetSlotLock());
    auto& slot =
        dut_->GetTransferRequestProcessor().GetRequestListLocked().GetSlot(kAdminCommandSlotNumber);
    EXPECT_EQ(slot.state, SlotState::kTimeout);
    // Completion callback must be reset so there are no lingering references to the wait context.
    EXPECT_FALSE(slot.completion_cb);
  }

  mock_device_.GetScsiCommandProcessor().Reset();
}

TEST_F(TimeoutTest, AdminCommandTimeoutStopsControllerAndClearsSlotOnDriverStop) {
  constexpr uint8_t kTestLun = 0;
  auto lun_id = Ufs::TranslateScsiLunToUfsLun(kTestLun);
  ASSERT_OK(lun_id);

  HookRead10ToStall(mock_device_);
  dut_->GetTransferRequestProcessor().SetTimeout(zx::msec(10));

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(kMockBlockSize, 0, &vmo));

  uint8_t cdb_buffer[16] = {};
  auto* cdb = reinterpret_cast<scsi::Read10CDB*>(cdb_buffer);
  cdb->opcode = scsi::Opcode::READ_10;
  cdb->transfer_length = htobe16(1);
  ScsiCommandUpiu upiu(cdb_buffer, 10, DataDirection::kDeviceToHost, kMockBlockSize);

  zx::result result =
      dut_->GetTransferRequestProcessor().SendAdminScsiCmd(upiu, lun_id.value(), vmo.borrow());
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.status_value(), ZX_ERR_TIMED_OUT);

  // No data-path I/O slots are active, while admin slot 31 remains quarantined in kTimeout with
  // its data VMO pinned and UTRLDBR[31] set.
  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetInflightIoCount(), 0u);
  EXPECT_TRUE(dut_->GetTransferRequestProcessor().GetIoSlotState().has_allocated_slots);
  {
    std::lock_guard<std::mutex> lock(dut_->GetTransferRequestProcessor().GetSlotLock());
    const RequestSlot& slot =
        dut_->GetTransferRequestProcessor().GetRequestListLocked().GetSlot(kAdminCommandSlotNumber);
    ASSERT_EQ(slot.state, SlotState::kTimeout);
    EXPECT_TRUE(slot.pmt.is_valid());
  }

  driver_stopped_ = true;
  ASSERT_OK(driver_test().StopDriver());

  EXPECT_FALSE(HostControllerEnableReg::Get().ReadFrom(&dut_->GetMmio()).host_controller_enable());
  EXPECT_FALSE(dut_->GetTransferRequestProcessor().GetIoSlotState().has_allocated_slots);
  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetQuarantinedPmtCountForTesting(), 0u);
  {
    std::lock_guard<std::mutex> lock(dut_->GetTransferRequestProcessor().GetSlotLock());
    const RequestSlot& slot =
        dut_->GetTransferRequestProcessor().GetRequestListLocked().GetSlot(kAdminCommandSlotNumber);
    EXPECT_EQ(slot.state, SlotState::kFree);
    EXPECT_FALSE(slot.pmt.is_valid());
  }

  mock_device_.GetScsiCommandProcessor().Reset();
}

TEST_F(TimeoutTest, AdminCommandTimeoutQuarantinesPmtWhenDisableHostControllerFailsOnStop) {
  constexpr uint8_t kTestLun = 0;
  constexpr uint32_t kPinnedPages = 2;
  const uint32_t kTransferBytes = kPinnedPages * zx_system_get_page_size();
  const uint16_t kTransferBlocks = static_cast<uint16_t>(kTransferBytes / kMockBlockSize);
  auto lun_id = Ufs::TranslateScsiLunToUfsLun(kTestLun);
  ASSERT_OK(lun_id);

  HookRead10ToStall(mock_device_);
  mock_device_.GetRegisterMmioProcessor().SetHook(RegisterMap::kHCE,
                                                  RegisterMmioProcessor::NoOpHandler);
  dut_->GetTransferRequestProcessor().SetTimeout(zx::msec(10));

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(kTransferBytes, 0, &vmo));

  uint8_t cdb_buffer[16] = {};
  auto* cdb = reinterpret_cast<scsi::Read10CDB*>(cdb_buffer);
  cdb->opcode = scsi::Opcode::READ_10;
  cdb->transfer_length = htobe16(kTransferBlocks);
  ScsiCommandUpiu upiu(cdb_buffer, 10, DataDirection::kDeviceToHost, kTransferBytes);

  zx::result result =
      dut_->GetTransferRequestProcessor().SendAdminScsiCmd(upiu, lun_id.value(), vmo.borrow());
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.status_value(), ZX_ERR_TIMED_OUT);
  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetInflightIoCount(), 0u);

  driver_stopped_ = true;
  ASSERT_OK(driver_test().StopDriver());

  EXPECT_FALSE(dut_->GetTransferRequestProcessor().GetIoSlotState().has_allocated_slots);
  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetQuarantinedPmtCountForTesting(), 1u);
  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetQuarantinedPageCountForTesting(), kPinnedPages);
  {
    std::lock_guard<std::mutex> lock(dut_->GetTransferRequestProcessor().GetSlotLock());
    const RequestSlot& slot =
        dut_->GetTransferRequestProcessor().GetRequestListLocked().GetSlot(kAdminCommandSlotNumber);
    EXPECT_EQ(slot.state, SlotState::kFree);
    EXPECT_FALSE(slot.pmt.is_valid());
  }

  mock_device_.GetRegisterMmioProcessor().Reset();
  mock_device_.GetScsiCommandProcessor().Reset();
}

}  // namespace ufs
