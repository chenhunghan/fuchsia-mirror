// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_NVME_FAKE_FAKE_CONTROLLER_H_
#define SRC_DEVICES_BLOCK_DRIVERS_NVME_FAKE_FAKE_CONTROLLER_H_

#include <fidl/fuchsia.hardware.pci/cpp/common_types.h>
#include <lib/fit/function.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <lib/zx/clock.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/result.h>

#include <atomic>
#include <map>
#include <mutex>
#include <optional>
#include <unordered_map>
#include <vector>

#include "src/devices/block/drivers/nvme/commands.h"
#include "src/devices/block/drivers/nvme/fake/fake-namespace.h"
#include "src/devices/block/drivers/nvme/fake/fake-registers.h"
#include "src/devices/block/drivers/nvme/nvme.h"
#include "src/devices/block/drivers/nvme/queue-pair.h"
#include "src/devices/block/drivers/nvme/queue.h"

namespace fake_nvme {

constexpr size_t kAdminQueueId = 0;

class FakeController {
 public:
  using CommandHandler = std::function<void(
      nvme::Submission& submission, const nvme::TransactionData& data, nvme::Completion& status)>;
  using AsyncCommandHandler =
      std::function<void(nvme::Submission& submission, const nvme::TransactionData& data)>;

  FakeController();
  void Reset();

  // Called when a write to the submission queue doorbell register occurs.
  // Invoked synchronously via MMIO callbacks while Nvme holds admin_lock_ (or on io_thread_).
  void HandleSubmission(size_t queue_id, size_t index,
                        nvme::Submission& submission) TA_NO_THREAD_SAFETY_ANALYSIS;
  // Called when a submission is finished.
  void SubmitCompletion(nvme::Completion& completion);

  // Add a default command handler (does not overwrite an existing test override).
  void AddDefaultAdminCommand(uint8_t opcode, CommandHandler handler) {
    std::lock_guard<std::mutex> lock(lock_);
    if (!async_admin_commands_.contains(opcode)) {
      admin_commands_.try_emplace(opcode, std::move(handler));
    }
  }
  // Add or override a command handler for the given admin opcode.
  void AddAdminCommand(uint8_t opcode, CommandHandler handler) {
    std::lock_guard<std::mutex> lock(lock_);
    async_admin_commands_.erase(opcode);
    admin_commands_.insert_or_assign(opcode, std::move(handler));
  }
  // Add an async admin command handler that does not automatically post a completion.
  void AddAsyncAdminCommand(uint8_t opcode, AsyncCommandHandler handler) {
    std::lock_guard<std::mutex> lock(lock_);
    admin_commands_.erase(opcode);
    async_admin_commands_.insert_or_assign(opcode, std::move(handler));
  }
  // Add or override a command handler for the given I/O opcode.
  void AddIoCommand(uint8_t opcode, CommandHandler handler) {
    std::lock_guard<std::mutex> lock(lock_);
    async_io_commands_.erase(opcode);
    io_commands_.insert_or_assign(opcode, std::move(handler));
  }
  // Add an async I/O command handler that does not automatically post a completion.
  void AddAsyncIoCommand(uint8_t opcode, AsyncCommandHandler handler) {
    std::lock_guard<std::mutex> lock(lock_);
    io_commands_.erase(opcode);
    async_io_commands_.insert_or_assign(opcode, std::move(handler));
  }

  // Called when one of the Admin Queue address registers is written to (while Nvme::Init() holds
  // admin_lock_).
  void UpdateAdminQueue() TA_NO_THREAD_SAFETY_ANALYSIS;

  // Add a namespace to this controller.
  void AddNamespace(uint32_t nsid, FakeNamespace& ns) {
    std::lock_guard<std::mutex> lock(lock_);
    namespaces_.erase(nsid);
    namespaces_.emplace(nsid, ns);
  }

  // Called by the test fixture to give us a pointer to the driver instance.
  // We use the driver instance to access data buffers and queues since the values written to the
  // register are fake values from fake_bti.
  void SetNvme(nvme::Nvme* nvme) { nvme_ = nvme; }

  void AddQueuePair(size_t queue_id, const nvme::Queue* completion_queue,
                    const nvme::Queue* submission_queue, uint16_t interrupt_vector = 0,
                    bool interrupt_enabled = true) {
    std::lock_guard<std::mutex> lock(lock_);
    if (completion_queue) {
      completion_queues_.insert_or_assign(
          queue_id,
          QueueState{
              .queue = completion_queue,
              .consumer_location = static_cast<uint16_t>(completion_queue->entry_count() - 1),
              .producer_location = 0,
              .interrupt_vector = interrupt_vector,
              .interrupt_enabled = interrupt_enabled,
          });
      if (queue_id != kAdminQueueId) {
        io_cq_interrupt_vector_ = interrupt_vector;
      }
    }
    if (submission_queue) {
      submission_queues_.insert_or_assign(queue_id, QueueState{
                                                        .queue = submission_queue,
                                                        .consumer_location = 0,
                                                        .producer_location = 0,
                                                    });
    }
  }

  FakeRegisters& registers() { return regs_; }
  nvme::Nvme* nvme() { return nvme_; }
  const std::map<uint32_t, FakeNamespace&>& namespaces() const {
    std::lock_guard<std::mutex> lock(lock_);
    return namespaces_;
  }

  // PCI & controller configuration knobs for testing.
  void SetMaxIrqs(uint32_t max_irqs) { max_irqs_ = max_irqs; }
  void SetSupportedIrqMode(fuchsia_hardware_pci::InterruptMode mode) { supported_irq_mode_ = mode; }
  void SetMaxDataTransfer(uint8_t mdts) { max_data_transfer_ = mdts; }
  uint8_t max_data_transfer() const { return max_data_transfer_; }

  zx_status_t SetInterruptMode(fuchsia_hardware_pci::InterruptMode mode,
                               uint32_t requested_irq_count);
  zx::result<fuchsia_hardware_pci::InterruptMode> ConfigureInterruptMode(
      uint32_t requested_irq_count);
  zx::result<zx::interrupt> MapInterrupt(uint32_t which_irq);
  zx_status_t SetBusMastering(bool enabled);

  // Test helpers for inspecting/exercising admin queue state under admin_lock_.
  zx::result<uint16_t> SubmitDummyAdminSubmission(nvme::Submission& submission);
  size_t admin_in_flight_count() const;

  // Inspection getters for unit tests.
  uint32_t configured_irq_count() const { return configured_irq_count_; }
  std::optional<uint32_t> mapped_irq_vector() const { return mapped_irq_vector_; }
  std::optional<uint16_t> io_cq_interrupt_vector() const {
    std::lock_guard<std::mutex> lock(lock_);
    return io_cq_interrupt_vector_;
  }
  bool bus_master_enabled() const { return bus_master_enabled_; }
  bool irq_configured_while_controller_ready() const {
    return irq_configured_while_controller_ready_;
  }
  bool irq_mapped_while_controller_ready() const { return irq_mapped_while_controller_ready_; }
  bool bme_enabled_while_controller_ready() const { return bme_enabled_while_controller_ready_; }
  bool bme_disabled_after_controller_disabled() const {
    return bme_disabled_after_controller_disabled_;
  }
  bool bme_disabled_after_normal_shutdown() const { return bme_disabled_after_normal_shutdown_; }
  bool saw_normal_shutdown() const { return saw_normal_shutdown_.load(); }

  // Returns IRQ number |index|, and creates it if it doesn't yet exist.
  zx::result<zx::interrupt> GetOrCreateInterrupt(size_t index);

 private:
  // Controller-side information about a queue.
  struct QueueState {
    const nvme::Queue* queue;
    // Maximum available slot to fill
    // For completions, this is the value written to completion doorbell.
    // For submissions, this is the index of the last submission we handled.
    uint16_t consumer_location = 0;
    // Next available slot to fill.
    // For completions, this is updated whenever we finish a txn.
    // For submissions, this is the value written to the submission doorbell.
    uint16_t producer_location = 0;
    // Only used by completion queues. Phase bit that should be sit
    // in completion queue entries so that the NVME driver consumes them.
    uint8_t phase = 1;
    uint16_t interrupt_vector = 0;
    bool interrupt_enabled = true;
  };
  // Controller-side information about an interrupt.
  class IrqState {
   public:
    explicit IrqState(zx::interrupt irq) : irq_(std::move(irq)) {}
    // Trigger the interrupt, or mark it as pending if it is disabled.
    void Trigger() {
      if (enabled_) {
        irq_.trigger(0, zx::clock::get_boot());
      } else {
        pending_ = true;
      }
    }
    // Enable the interrupt and trigger any pending interrupts.
    void Enable() {
      enabled_ = true;
      if (pending_) {
        Trigger();
        pending_ = false;
      }
    }
    // Disable the interrupt.
    void Disable() { enabled_ = false; }
    zx::unowned_interrupt irq() { return irq_.borrow(); }

   private:
    // Is this interrupt enabled?
    bool enabled_ = true;
    // Was this interrupt triggered while it was disabled?
    bool pending_ = false;
    // The actual interrupt object.
    zx::interrupt irq_;
  };

  void SetConfig(nvme::ControllerConfigReg& cfg);
  void UpdateIrqMask(bool enable, nvme::InterruptReg& state);
  void RingDoorbell(bool is_submit, size_t queue_id, nvme::DoorbellReg& reg);

  zx::result<zx::interrupt> GetOrCreateInterruptLocked(size_t index) TA_REQ(lock_);
  QueueState& GetQueueState(size_t queue_id, std::unordered_map<size_t, QueueState>* queues);

  // Used to maintain integrity of the map containers rather than their contents, which are used to
  // add more elements to the map in certain cases (e.g., processing a queue's command to set up
  // another queue).
  mutable std::mutex lock_;
  std::unordered_map<size_t, QueueState> completion_queues_ TA_GUARDED(lock_);
  std::unordered_map<size_t, QueueState> submission_queues_ TA_GUARDED(lock_);
  std::unordered_map<size_t, IrqState> irqs_ TA_GUARDED(lock_);
  std::unordered_map<uint8_t, CommandHandler> admin_commands_ TA_GUARDED(lock_);
  std::unordered_map<uint8_t, AsyncCommandHandler> async_admin_commands_ TA_GUARDED(lock_);
  std::unordered_map<uint8_t, CommandHandler> io_commands_ TA_GUARDED(lock_);
  std::unordered_map<uint8_t, AsyncCommandHandler> async_io_commands_ TA_GUARDED(lock_);
  // This is ordered because "Get Active Namespaces" returns an ordered list of namespaces.
  std::map<uint32_t, FakeNamespace&> namespaces_ TA_GUARDED(lock_);
  std::optional<uint16_t> io_cq_interrupt_vector_ TA_GUARDED(lock_);
  nvme::Nvme* nvme_ = nullptr;

  uint32_t max_irqs_ = 2;
  fuchsia_hardware_pci::InterruptMode supported_irq_mode_ =
      fuchsia_hardware_pci::InterruptMode::kMsiX;
  uint32_t configured_irq_count_ = 0;
  std::optional<uint32_t> mapped_irq_vector_;
  uint8_t max_data_transfer_ = 2;
  std::atomic<bool> saw_normal_shutdown_ = false;
  bool bus_master_enabled_ = false;
  bool irq_configured_while_controller_ready_ = false;
  bool irq_mapped_while_controller_ready_ = false;
  bool bme_enabled_while_controller_ready_ = false;
  bool bme_disabled_after_controller_disabled_ = false;
  bool bme_disabled_after_normal_shutdown_ = false;

  FakeRegisters regs_;
  NvmeRegisterCallbacks callbacks_{
      .set_config = fit::bind_member(this, &FakeController::SetConfig),
      .interrupt_mask_update = fit::bind_member(this, &FakeController::UpdateIrqMask),
      .doorbell_ring = fit::bind_member(this, &FakeController::RingDoorbell),
      .admin_queue_update = fit::bind_member(this, &FakeController::UpdateAdminQueue),
  };
};

}  // namespace fake_nvme

#endif  // SRC_DEVICES_BLOCK_DRIVERS_NVME_FAKE_FAKE_CONTROLLER_H_
