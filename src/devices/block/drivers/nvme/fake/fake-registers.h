// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_NVME_FAKE_FAKE_REGISTERS_H_
#define SRC_DEVICES_BLOCK_DRIVERS_NVME_FAKE_FAKE_REGISTERS_H_

#include <lib/driver/mmio/cpp/mmio-buffer.h>
#include <lib/driver/mmio/testing/cpp/test-helper.h>
#include <lib/mmio-ptr/fake.h>
#include <lib/zircon-internal/thread_annotations.h>

#include <atomic>
#include <functional>
#include <mutex>
#include <unordered_map>
#include <vector>

#include "src/devices/block/drivers/nvme/registers.h"

namespace fake_nvme {

struct NvmeRegisterCallbacks {
  std::function<void(nvme::ControllerConfigReg&)> set_config;
  std::function<void(bool enable, nvme::InterruptReg&)> interrupt_mask_update;
  std::function<void(bool is_submit, size_t queue_id, nvme::DoorbellReg&)> doorbell_ring;
  std::function<void()> admin_queue_update;
};

// Implements fake MMIO support for the NVMe controller registers.
class FakeRegisters {
 public:
  enum class IrqOrderingEventType {
    kMaskSet,
    kIoCqDoorbell,
    kMaskClear,
  };
  struct IrqOrderingEvent {
    IrqOrderingEventType type;
    uint32_t value;
  };

  FakeRegisters();
  void Reset();

  fdf::MmioBuffer GetBuffer() {
    return fdf_testing::CreateMmioBuffer(nvme::NVME_REG_DOORBELL_BASE + 0x100,
                                         ZX_CACHE_POLICY_UNCACHED, &kMmioOps, this);
  }

  void SetCallbacks(NvmeRegisterCallbacks* callbacks) { callbacks_ = callbacks; }

  void SetUpCompletionDoorbell(size_t index) {
    std::lock_guard<std::mutex> lock(doorbells_lock_);
    completion_doorbells_.insert_or_assign(index, nvme::DoorbellReg{});
  }

  void SetUpSubmissionDoorbell(size_t index) {
    std::lock_guard<std::mutex> lock(doorbells_lock_);
    submission_doorbells_.insert_or_assign(index, nvme::DoorbellReg{});
  }

  nvme::CapabilityReg& caps() { return caps_; }
  nvme::ControllerConfigReg& ccfg() { return ccfg_; }
  nvme::ControllerStatusReg& csts() { return csts_; }
  size_t admin_queue_addr_reads() const { return admin_queue_addr_reads_.load(); }

  std::vector<IrqOrderingEvent> irq_ordering_events() const {
    std::lock_guard<std::mutex> lock(events_lock_);
    return irq_ordering_events_;
  }

  void ClearIrqOrderingEvents() {
    std::lock_guard<std::mutex> lock(events_lock_);
    irq_ordering_events_.clear();
  }

 private:
  nvme::CapabilityReg caps_;
  nvme::VersionReg vers_;
  nvme::InterruptReg interrupt_mask_set_;
  nvme::InterruptReg interrupt_mask_clear_;
  nvme::ControllerConfigReg ccfg_;
  nvme::ControllerStatusReg csts_;
  nvme::AdminQueueAttributesReg admin_queue_attrs_;
  nvme::AdminQueueAddressReg admin_submission_queue_;
  nvme::AdminQueueAddressReg admin_completion_queue_;

  std::mutex doorbells_lock_;
  std::unordered_map<size_t, nvme::DoorbellReg> completion_doorbells_ TA_GUARDED(doorbells_lock_);
  std::unordered_map<size_t, nvme::DoorbellReg> submission_doorbells_ TA_GUARDED(doorbells_lock_);
  std::atomic<size_t> admin_queue_addr_reads_ = 0;

  mutable std::mutex events_lock_;
  std::vector<IrqOrderingEvent> irq_ordering_events_ TA_GUARDED(events_lock_);

  // Define read/write for |bits| that just crashes.
#define STUB_IO_OP(bits)                                                                          \
  static void Write##bits(const void* ctx, const fdf::MmioBuffer& mmio, uint##bits##_t val,       \
                          zx_off_t offs) {                                                        \
    ZX_ASSERT(false);                                                                             \
  }                                                                                               \
  static uint##bits##_t Read##bits(const void* ctx, const fdf::MmioBuffer& mmio, zx_off_t offs) { \
    ZX_ASSERT(false);                                                                             \
  }

  STUB_IO_OP(16)
  STUB_IO_OP(8)
#undef STUB_IO_OP

  static void Write64(const void* ctx, const fdf::MmioBuffer& mmio, uint64_t val, zx_off_t offs) {
    const_cast<FakeRegisters*>(static_cast<const FakeRegisters*>(ctx))->Write64(val, offs);
  }
  static uint64_t Read64(const void* ctx, const fdf::MmioBuffer& mmio, zx_off_t offs) {
    return const_cast<FakeRegisters*>(static_cast<const FakeRegisters*>(ctx))->Read64(offs);
  }
  void Write64(uint64_t val, zx_off_t offs);
  uint64_t Read64(zx_off_t offs);

  static void Write32(const void* ctx, const fdf::MmioBuffer& mmio, uint32_t val, zx_off_t offs) {
    const_cast<FakeRegisters*>(static_cast<const FakeRegisters*>(ctx))->Write32(val, offs);
  }
  static uint32_t Read32(const void* ctx, const fdf::MmioBuffer& mmio, zx_off_t offs) {
    return const_cast<FakeRegisters*>(static_cast<const FakeRegisters*>(ctx))->Read32(offs);
  }
  void Write32(uint32_t val, zx_off_t offs);
  uint32_t Read32(zx_off_t offs);

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
  NvmeRegisterCallbacks* callbacks_ = nullptr;
};

}  // namespace fake_nvme

#endif  // SRC_DEVICES_BLOCK_DRIVERS_NVME_FAKE_FAKE_REGISTERS_H_
