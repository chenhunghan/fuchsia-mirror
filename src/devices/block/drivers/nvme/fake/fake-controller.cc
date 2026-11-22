// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/drivers/nvme/fake/fake-controller.h"

#include <lib/stdcompat/span.h>
#include <lib/zx/clock.h>

#include "src/devices/block/drivers/nvme/commands.h"
#include "src/devices/block/drivers/nvme/registers.h"

namespace fake_nvme {

FakeController::FakeController() { regs_.SetCallbacks(&callbacks_); }

void FakeController::Reset() {
  {
    std::lock_guard<std::mutex> lock(lock_);
    completion_queues_.clear();
    submission_queues_.clear();
    irqs_.clear();
    admin_commands_.clear();
    async_admin_commands_.clear();
    io_commands_.clear();
    async_io_commands_.clear();
    namespaces_.clear();
    io_cq_interrupt_vector_ = std::nullopt;
  }
  nvme_ = nullptr;
  regs_.Reset();
  regs_.SetCallbacks(&callbacks_);
  max_irqs_ = 2;
  supported_irq_mode_ = fuchsia_hardware_pci::InterruptMode::kMsiX;
  configured_irq_count_ = 0;
  mapped_irq_vector_ = std::nullopt;
  max_data_transfer_ = 2;
  saw_normal_shutdown_.store(false);
  bus_master_enabled_ = false;
  irq_configured_while_controller_ready_ = false;
  irq_mapped_while_controller_ready_ = false;
  bme_enabled_while_controller_ready_ = false;
  bme_disabled_after_controller_disabled_ = false;
  bme_disabled_after_normal_shutdown_ = false;
}

void FakeController::HandleSubmission(size_t queue_id, size_t index, nvme::Submission& submission) {
  // Fill in the completion with data we know will be returned.
  nvme::Completion completion{};
  completion.set_command_id(static_cast<uint16_t>(submission.cid()));
  completion.set_sq_id(static_cast<uint16_t>(queue_id));
  completion.set_sq_head(static_cast<uint16_t>(index));

  // Try and find the command handler. If it is found, run it, otherwise, return an error.
  CommandHandler cmd_handler;
  AsyncCommandHandler async_handler;
  {
    std::lock_guard<std::mutex> lock(lock_);
    const uint8_t opcode = static_cast<uint8_t>(submission.opcode());
    auto& async_set = (queue_id == kAdminQueueId) ? async_admin_commands_ : async_io_commands_;
    if (auto async_it = async_set.find(opcode); async_it != async_set.end()) {
      async_handler = async_it->second;
    } else {
      auto& command_set = (queue_id == kAdminQueueId) ? admin_commands_ : io_commands_;
      if (auto cmd = command_set.find(opcode); cmd != command_set.end()) {
        cmd_handler = cmd->second;
      }
    }
  }

  auto& txn_data =
      (queue_id == kAdminQueueId) ? nvme_->admin_queue_->txn_data() : nvme_->io_queue_->txn_data();
  if (async_handler) {
    async_handler(submission, txn_data[submission.cid()]);
    return;
  }

  if (cmd_handler) {
    cmd_handler(submission, txn_data[submission.cid()], completion);
  } else {
    // Command did not exist, return an error.
    completion.set_status_code_type(nvme::StatusCodeType::kGeneric)
        .set_status_code(nvme::GenericStatus::kInvalidOpcode);
  }
  SubmitCompletion(completion);
}

FakeController::QueueState& FakeController::GetQueueState(
    size_t queue_id, std::unordered_map<size_t, QueueState>* queues) {
  std::lock_guard<std::mutex> lock(lock_);
  auto iter = queues->find(queue_id);
  ZX_ASSERT(iter != queues->end());
  ZX_ASSERT(iter->second.queue);
  return iter->second;
}

void FakeController::SubmitCompletion(nvme::Completion& completion) {
  std::lock_guard<std::mutex> lock(lock_);
  auto iter = completion_queues_.find(completion.sq_id());
  ZX_ASSERT(iter != completion_queues_.end());
  QueueState& queue = iter->second;
  ZX_ASSERT(queue.queue);

  // Check there's space.
  ZX_ASSERT((queue.producer_location + 1) % queue.queue->entry_count() != queue.consumer_location);

  // Mark the completion as ready to be read.
  completion.set_phase(queue.phase);

  // Insert the completion into the queue.
  nvme::Completion* ptr = static_cast<nvme::Completion*>(queue.queue->head());
  ptr[queue.producer_location] = completion;

  // Move forward through the queue.
  queue.producer_location++;
  if (queue.producer_location == queue.queue->entry_count()) {
    queue.producer_location = 0;
    queue.phase ^= 1;
  }

  if (queue.interrupt_enabled) {
    if (auto irq_it = irqs_.find(queue.interrupt_vector); irq_it != irqs_.end()) {
      irq_it->second.Trigger();
    }
  }
}

zx::result<zx::interrupt> FakeController::GetOrCreateInterruptLocked(size_t index) {
  auto value = irqs_.find(index);
  zx::unowned_interrupt to_clone;
  if (value != irqs_.end()) {
    to_clone = value->second.irq();
  } else {
    zx::interrupt irq;
    zx_status_t status = zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &irq);
    if (status != ZX_OK) {
      return zx::error(status);
    }
    to_clone = irqs_.emplace(index, IrqState(std::move(irq))).first->second.irq();
  }

  zx::interrupt out;
  zx_status_t status = to_clone->duplicate(ZX_RIGHT_SAME_RIGHTS, &out);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(std::move(out));
}

zx::result<zx::interrupt> FakeController::GetOrCreateInterrupt(size_t index) {
  std::lock_guard<std::mutex> lock(lock_);
  return GetOrCreateInterruptLocked(index);
}

zx_status_t FakeController::SetInterruptMode(fuchsia_hardware_pci::InterruptMode mode,
                                             uint32_t requested_irq_count) {
  if (regs_.csts().ready() || regs_.ccfg().enabled()) {
    irq_configured_while_controller_ready_ = true;
  }
  if (mode != supported_irq_mode_ || requested_irq_count == 0 || requested_irq_count > max_irqs_) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  if (mode == fuchsia_hardware_pci::InterruptMode::kMsiX ||
      mode == fuchsia_hardware_pci::InterruptMode::kMsi) {
    // Mirror the PCI bus driver enabling Bus Mastering when configuring MSI/MSI-X.
    bus_master_enabled_ = true;
  }
  configured_irq_count_ = requested_irq_count;
  std::lock_guard<std::mutex> lock(lock_);
  for (uint32_t i = 0; i < requested_irq_count; ++i) {
    auto irq = GetOrCreateInterruptLocked(i);
    if (irq.is_error()) {
      return irq.status_value();
    }
  }
  return ZX_OK;
}

zx::result<fuchsia_hardware_pci::InterruptMode> FakeController::ConfigureInterruptMode(
    uint32_t requested_irq_count) {
  zx_status_t status = SetInterruptMode(supported_irq_mode_, requested_irq_count);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(supported_irq_mode_);
}

zx::result<zx::interrupt> FakeController::MapInterrupt(uint32_t which_irq) {
  if (regs_.csts().ready() || regs_.ccfg().enabled()) {
    irq_mapped_while_controller_ready_ = true;
  }
  if (which_irq >= configured_irq_count_) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  auto irq = GetOrCreateInterrupt(which_irq);
  if (irq.is_error()) {
    return irq.take_error();
  }
  mapped_irq_vector_ = which_irq;
  return irq;
}

zx_status_t FakeController::SetBusMastering(bool enabled) {
  if (enabled) {
    if (regs_.csts().ready() || regs_.ccfg().enabled()) {
      bme_enabled_while_controller_ready_ = true;
    }
  } else {
    bme_disabled_after_controller_disabled_ = !regs_.ccfg().enabled() && !regs_.csts().ready();
    bme_disabled_after_normal_shutdown_ = saw_normal_shutdown_.load();
  }
  bus_master_enabled_ = enabled;
  return ZX_OK;
}

zx::result<uint16_t> FakeController::SubmitDummyAdminSubmission(nvme::Submission& submission) {
  std::lock_guard<std::mutex> lock(nvme_->admin_lock_);
  return nvme_->admin_queue_->Submit(submission, std::nullopt, 0, 0);
}

size_t FakeController::admin_in_flight_count() const {
  std::lock_guard<std::mutex> lock(nvme_->admin_lock_);
  return nvme_->admin_queue_ ? nvme_->admin_queue_->in_flight_count() : 0;
}

void FakeController::SetConfig(nvme::ControllerConfigReg& cfg) {
  if (cfg.shutdown_notification() == nvme::ControllerConfigReg::ShutdownNotification::kNormal ||
      cfg.shutdown_notification() == nvme::ControllerConfigReg::ShutdownNotification::kAbrupt) {
    if (cfg.shutdown_notification() == nvme::ControllerConfigReg::ShutdownNotification::kNormal) {
      saw_normal_shutdown_.store(true);
    }
    regs_.csts().set_shutdown_status(nvme::ControllerStatusReg::ShutdownStatus::kComplete);
  }
  if (cfg.enabled()) {
    regs_.csts().set_ready(true);
  } else {
    regs_.csts().set_ready(false);
    regs_.csts().set_controller_fatal_status(false);
    regs_.csts().set_shutdown_status(nvme::ControllerStatusReg::ShutdownStatus::kNoShutdown);
  }
}

void FakeController::UpdateIrqMask(bool enable, nvme::InterruptReg& state) {
  std::lock_guard<std::mutex> lock(lock_);
  uint32_t val = state.reg_value();
  for (size_t i = 0; i < 32; i++) {
    if ((val >> i) & 1) {
      auto entry = irqs_.find(i);
      ZX_ASSERT(entry != irqs_.end());
      if (enable) {
        entry->second.Enable();
      } else {
        entry->second.Disable();
      }
    }
  }
}

void FakeController::RingDoorbell(bool is_submit, size_t queue_id, nvme::DoorbellReg& reg) {
  if (!is_submit) {
    // Completion is easy, just note the new location.
    QueueState& queue = GetQueueState(queue_id, &completion_queues_);
    queue.consumer_location = static_cast<uint16_t>(reg.value());
    return;
  }

  // Submission is a little more complex. For each new submission (between the old and new values of
  // the doorbell), we call HandleSubmission().
  QueueState& queue = GetQueueState(queue_id, &submission_queues_);
  uint16_t old = queue.consumer_location;

  cpp20::span<nvme::Submission> submissions(static_cast<nvme::Submission*>(queue.queue->head()),
                                            queue.queue->entry_count());

  size_t i = old;
  auto next = [&i, &submissions]() {
    i++;
    if (i == submissions.size()) {
      i = 0;
    }
  };

  for (; i != reg.value(); next()) {
    queue.consumer_location = static_cast<uint16_t>(i);
    HandleSubmission(queue_id, i, submissions[i]);
  }
  queue.consumer_location = static_cast<uint16_t>(i);
}

void FakeController::UpdateAdminQueue() {
  AddQueuePair(0, const_cast<nvme::Queue*>(&nvme_->admin_queue_->completion()),
               const_cast<nvme::Queue*>(&nvme_->admin_queue_->submission()));
}

}  // namespace fake_nvme
