// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/drivers/nvme/nvme.h"

#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/mmio/cpp/mmio-buffer.h>
#include <lib/fit/defer.h>
#include <lib/fzl/vmo-mapper.h>
#include <lib/sync/completion.h>
#include <lib/zx/clock.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <threads.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>
#include <zircon/threads.h>
#include <zircon/types.h>

#include <algorithm>
#include <limits>
#include <mutex>

#include <hwreg/bitfields.h>

#include "src/devices/block/drivers/nvme/commands/features.h"
#include "src/devices/block/drivers/nvme/commands/identify.h"
#include "src/devices/block/drivers/nvme/commands/nvme-io.h"
#include "src/devices/block/drivers/nvme/commands/queue.h"
#include "src/devices/block/drivers/nvme/namespace.h"
#include "src/devices/block/drivers/nvme/registers.h"

namespace nvme {

// For safety - so that the driver doesn't draw too many resources.
constexpr size_t kMaxNamespacesToBind = 4;

// c.f. NVMe Base Specification 2.0, section 3.1.3.8 "AQA - Admin Queue Attributes"
constexpr size_t kAdminQueueMaxEntries = 4096;

constexpr uint32_t kDefaultResetWaitMs = 5000;
constexpr uint32_t kMinResetWaitMs = 500;
constexpr uint32_t kMaxResetWaitMs = 30000;
constexpr zx::duration kAdminCommandLogInterval = zx::sec(10);

static uint32_t GetControllerTimeoutMs(const CapabilityReg& caps_reg) {
  const uint32_t cap_timeout_ms = caps_reg.timeout_ms();
  if (cap_timeout_ms == 0) {
    return kDefaultResetWaitMs;
  }
  return std::clamp(cap_timeout_ms, kMinResetWaitMs, kMaxResetWaitMs);
}

// TODO(https://fxbug.dev/42053036): Consider using dedicated interrupt (and IO thread) per
// namespace/queue.
int Nvme::IrqLoop() {
  while (true) {
    zx_status_t status = irq_.wait(nullptr);
    if (status != ZX_OK) {
      if (status == ZX_ERR_CANCELED) {
        fdf::info("Interrupt cancelled. Exiting IRQ loop.");
      } else {
        fdf::error("Failed to wait for interrupt: {}", zx_status_get_string(status));
      }
      break;
    }

    interrupt_count_property_.Add(1);

    // The interrupt mask registers must only be accessed when not using MSI-X (NVMe Base Spec 2.0,
    // sections 3.1.3.3 and 3.1.3.4). Per section 7.5.1.1, leave the vector masked until IoLoop()
    // drains completions and updates CQHDBL so pin-based INTx cannot immediately re-assert.
    if (irq_mode_ != fuchsia_hardware_pci::InterruptMode::kMsiX) {
      std::lock_guard<std::mutex> lock(legacy_irq_lock_);
      InterruptReg::MaskSet().FromValue(uint32_t{1} << io_irq_vector_).WriteTo(&*mmio_);
      legacy_irq_masked_ = true;
      ++legacy_irq_mask_gen_;
    }

    sync_completion_signal(&io_signal_);
  }
  return thrd_success;
}

zx::result<Completion> Nvme::DoAdminCommandSync(Submission& submission,
                                                std::optional<zx::unowned_vmo> admin_data) {
  zx_status_t status;
  std::lock_guard<std::mutex> lock(admin_lock_);
  if (driver_shutdown_.load(std::memory_order_relaxed) || !admin_queue_) {
    return zx::error(ZX_ERR_CANCELED);
  }

  uint64_t data_size = 0;
  if (admin_data.has_value()) {
    status = admin_data.value()->get_size(&data_size);
    if (status != ZX_OK) {
      fdf::error("Failed to get size of vmo: {}", zx_status_get_string(status));
      return zx::error(status);
    }
  }

  zx::result<uint16_t> submit_result = admin_queue_->Submit(submission, admin_data, 0, data_size);
  if (submit_result.is_error()) {
    fdf::error("Failed to submit admin command: {}", submit_result.status_string());
    return submit_result.take_error();
  }
  const uint16_t cid = submit_result.value();

  const zx::time start = zx::clock::get_monotonic();
  zx::time next_log_time = start + kAdminCommandLogInterval;
  while (true) {
    Completion* completion = nullptr;
    status = admin_queue_->CheckForNewCompletion(&completion);
    if (status == ZX_ERR_SHOULD_WAIT) {
      const zx::time now = zx::clock::get_monotonic();
      if (now >= next_log_time) {
        fdf::error("Still waiting after {} s for admin command completion (cid={})...",
                   (now - start).to_secs(), cid);
        next_log_time = now + kAdminCommandLogInterval;
      }

      zx::nanosleep(zx::deadline_after(zx::msec(1)));
      continue;
    }

    // Copy the CQE before ringing CQHDBL, which releases the slot back to controller DMA.
    const Completion cqe = *completion;
    admin_queue_->RingCompletionDb();

    if (status != ZX_OK) {
      fdf::warn("Ignoring invalid admin completion ({}) while waiting for cid={}.",
                zx_status_get_string(status), cid);
      continue;
    }

    if (cqe.command_id() != cid) {
      fdf::warn("Ignoring mismatched admin completion cid={} (expected cid={}).", cqe.command_id(),
                cid);
      continue;
    }

    if (cqe.status_code_type() == StatusCodeType::kGeneric && cqe.status_code() == 0) {
      fdf::trace("Completed admin command (cid={}) OK.", cid);
      return zx::ok(cqe);
    }

    fdf::error("Completed admin command (cid={}) ERROR: status type={:01x}, status={:02x}", cid,
               static_cast<uint32_t>(cqe.status_code_type()), cqe.status_code());
    return zx::error(ZX_ERR_IO);
  }
}

void Nvme::ProcessIoSubmissions() {
  while (true) {
    IoCommand* io_cmd;
    {
      std::lock_guard<std::mutex> lock(commands_lock_);
      io_cmd = list_remove_head_type(&pending_commands_, IoCommand, node);
    }

    if (io_cmd == nullptr) {
      return;
    }

    zx_status_t status;
    switch (io_cmd->operation.tag) {
      case block_server::Operation::Tag::Flush: {
        NvmIoFlushSubmission submission;
        submission.namespace_id = io_cmd->namespace_id;
        status = io_queue_->Submit(submission, std::nullopt, 0, 0, io_cmd).status_value();
        break;
      }
      case block_server::Operation::Tag::Read:
      case block_server::Operation::Tag::Write: {
        bool is_write = io_cmd->operation.tag == block_server::Operation::Tag::Write;
        NvmIoSubmission submission(is_write);
        submission.namespace_id = io_cmd->namespace_id;

        uint64_t device_block_offset = io_cmd->operation.read.device_block_offset;
        uint32_t block_count = io_cmd->operation.read.block_count;
        uint64_t vmo_offset = io_cmd->operation.read.vmo_offset;
        bool force_access =
            is_write ? io_cmd->operation.write.options.flags.is_force_access() : false;

        submission.set_start_lba(device_block_offset).set_block_count(block_count - 1);
        if (force_access) {
          submission.set_force_unit_access(true);
        }

        status = io_queue_
                     ->Submit(submission, zx::unowned_vmo(io_cmd->vmo->get()), vmo_offset,
                              block_count * io_cmd->block_size_bytes, io_cmd)
                     .status_value();
        break;
      }
      default:
        fdf::error("Unsupported operation tag: {}", static_cast<uint32_t>(io_cmd->operation.tag));
        io_cmd->Complete(ZX_ERR_NOT_SUPPORTED);
        continue;
    }
    switch (status) {
      case ZX_OK:
        break;
      case ZX_ERR_SHOULD_WAIT:
        // We can't proceed if there is no available space in the submission queue. Put command back
        // at front of queue for further processing later.
        {
          std::lock_guard<std::mutex> lock(commands_lock_);
          list_add_head(&pending_commands_, &io_cmd->node);
        }
        return;
      default:
        fdf::error("Failed to submit transaction (command {}): {}",
                   static_cast<const void*>(io_cmd), zx_status_get_string(status));
        io_cmd->Complete(status == ZX_ERR_INVALID_ARGS ? ZX_ERR_INVALID_ARGS : ZX_ERR_INTERNAL);
        break;
    }
  }
}

void Nvme::ProcessIoCompletions() {
  bool ring_doorbell = false;
  while (true) {
    Completion* completion = nullptr;
    IoCommand* io_cmd = nullptr;
    zx_status_t status = io_queue_->CheckForNewCompletion(&completion, &io_cmd);
    if (status == ZX_ERR_SHOULD_WAIT) {
      break;
    }
    ring_doorbell = true;
    if (status != ZX_OK) {
      fdf::error("Failed to check for new IO completion: {}", zx_status_get_string(status));
      continue;
    }

    if (io_cmd == nullptr) {
      fdf::error("Completed transaction isn't associated with a command.");
      continue;
    }

    if (completion->status_code_type() == StatusCodeType::kGeneric &&
        completion->status_code() == 0) {
      fdf::trace("Completed transaction #{} command {} OK.", completion->command_id(),
                 static_cast<const void*>(io_cmd));
      io_cmd->Complete(ZX_OK);
    } else {
      fdf::warn("Completed transaction #{} command {} ERROR: status type={:01x}, status={:02x}",
                completion->command_id(), static_cast<const void*>(io_cmd),
                static_cast<uint32_t>(completion->status_code_type()), completion->status_code());
      io_cmd->Complete(ZX_ERR_IO);
    }
  }

  if (ring_doorbell) {
    io_queue_->RingCompletionDb();
  }
}

int Nvme::IoLoop() {
  while (true) {
    if (driver_shutdown_.load(std::memory_order_acquire)) {
      fdf::debug("IO thread exiting.");
      break;
    }

    zx_status_t status = sync_completion_wait(&io_signal_, ZX_TIME_INFINITE);
    if (status != ZX_OK) {
      fdf::error("Failed to wait for sync completion: {}", zx_status_get_string(status));
      break;
    }
    if (driver_shutdown_.load(std::memory_order_acquire)) {
      fdf::debug("IO thread exiting.");
      break;
    }
    sync_completion_reset(&io_signal_);

    bool was_masked = false;
    uint64_t mask_gen = 0;
    if (irq_mode_ != fuchsia_hardware_pci::InterruptMode::kMsiX) {
      std::lock_guard<std::mutex> lock(legacy_irq_lock_);
      was_masked = legacy_irq_masked_;
      mask_gen = legacy_irq_mask_gen_;
    }

    // Process completion messages and ring CQHDBL before unmasking non-MSI-X interrupts.
    ProcessIoCompletions();
    if (was_masked) {
      std::lock_guard<std::mutex> lock(legacy_irq_lock_);
      if (legacy_irq_masked_ && legacy_irq_mask_gen_ == mask_gen) {
        InterruptReg::MaskClear().FromValue(uint32_t{1} << io_irq_vector_).WriteTo(&*mmio_);
        legacy_irq_masked_ = false;
      }
    }

    // process work queue
    ProcessIoSubmissions();
  }
  return thrd_success;
}

void Nvme::QueueIoCommand(IoCommand* io_cmd) {
  bool shutdown = false;
  {
    std::lock_guard<std::mutex> lock(commands_lock_);
    if (driver_shutdown_.load(std::memory_order_relaxed)) {
      shutdown = true;
    } else {
      list_add_tail(&pending_commands_, &io_cmd->node);
    }
  }
  if (shutdown) {
    io_cmd->Complete(ZX_ERR_PEER_CLOSED);
    return;
  }

  sync_completion_signal(&io_signal_);
}

void Nvme::Stop(fdf::StopCompleter completer) {
  fdf::debug("Preparing to stop driver.");

  // Tear down hardware and complete/cancel all in-flight and queued requests before waiting for
  // BlockServer sessions to terminate so a wedged controller cannot hang StopBlockServer().
  PerformTeardown();

  if (namespaces_.empty()) {
    completer(zx::ok());
    return;
  }

  struct Context {
    Context(size_t count, fdf::StopCompleter comp)
        : pending_count(count), completer(std::move(comp)) {}
    std::atomic<size_t> pending_count;
    fdf::StopCompleter completer;
  };

  auto ctx = std::make_shared<Context>(namespaces_.size(), std::move(completer));

  for (auto& ns : namespaces_) {
    ns->StopBlockServer([ctx]() {
      if (ctx->pending_count.fetch_sub(1) == 1) {
        ctx->completer(zx::ok());
      }
    });
  }
}

Nvme::~Nvme() { PerformTeardown(); }

static zx_status_t WaitForReset(bool desired_ready_state, const CapabilityReg& caps_reg,
                                fdf::MmioBuffer* mmio) {
  const uint32_t timeout_ms = GetControllerTimeoutMs(caps_reg);
  const zx::time start = zx::clock::get_monotonic();
  zx::time next_log_time = start + zx::msec(timeout_ms);
  while (true) {
    ControllerStatusReg csts = ControllerStatusReg::Get().ReadFrom(mmio);
    // Per NVMe Base Spec 2.0 section 3.1.3.6, CSTS.CFS clears alongside CSTS.RDY -> 0 during
    // controller reset; only abort early on CFS when waiting for the controller to become ready.
    if (desired_ready_state && csts.controller_fatal_status()) {
      fdf::error("Controller reported fatal status (CSTS.CFS=1) while waiting for ready state.");
      return ZX_ERR_IO;
    }
    if (csts.ready() == desired_ready_state) {
      fdf::debug("Controller reached ready state {} (took {} ms).", desired_ready_state,
                 (zx::clock::get_monotonic() - start).to_msecs());
      return ZX_OK;
    }
    const zx::time now = zx::clock::get_monotonic();
    if (now >= next_log_time) {
      fdf::error("Still waiting after {} ms for controller ready state {}...",
                 (now - start).to_msecs(), desired_ready_state);
      next_log_time = now + zx::msec(timeout_ms);
    }
    zx::nanosleep(zx::deadline_after(zx::msec(1)));
  }
}

void Nvme::PerformTeardown() {
  if (torn_down_) {
    return;
  }
  torn_down_ = true;

  {
    std::scoped_lock lock(admin_lock_, commands_lock_);
    driver_shutdown_.store(true, std::memory_order_release);
  }
  if (irq_.is_valid()) {
    irq_.destroy();  // Make irq_.wait() in IrqLoop() return ZX_ERR_CANCELED.
  }
  if (irq_thread_started_) {
    thrd_join(irq_thread_, nullptr);
    irq_thread_started_ = false;
  }
  if (io_thread_started_) {
    sync_completion_signal(&io_signal_);
    thrd_join(io_thread_, nullptr);
    io_thread_started_ = false;
  }

  // Harvest any completions the controller already posted before shutting it down.
  if (io_queue_) {
    ProcessIoCompletions();
  }

  if (mmio_.has_value()) {
    const CapabilityReg caps_reg = CapabilityReg::Get().ReadFrom(&*mmio_);
    ControllerConfigReg cc = ControllerConfigReg::Get().ReadFrom(&*mmio_);
    ControllerStatusReg csts = ControllerStatusReg::Get().ReadFrom(&*mmio_);
    if (cc.enabled() && csts.ready() && !csts.controller_fatal_status()) {
      // NVMe Base Spec 2.0 section 7.6.2: issue normal shutdown notification and wait for
      // CSTS.SHST == kComplete prior to disabling the controller and bus mastering.
      cc.set_shutdown_notification(ControllerConfigReg::ShutdownNotification::kNormal)
          .WriteTo(&*mmio_);
      const uint32_t timeout_ms = GetControllerTimeoutMs(caps_reg);
      const zx::time start = zx::clock::get_monotonic();
      zx::time next_log_time = start + zx::msec(timeout_ms);
      while (true) {
        csts = ControllerStatusReg::Get().ReadFrom(&*mmio_);
        if (csts.shutdown_status() == ControllerStatusReg::ShutdownStatus::kComplete ||
            csts.controller_fatal_status()) {
          break;
        }
        const zx::time now = zx::clock::get_monotonic();
        if (now >= next_log_time) {
          fdf::error("Still waiting after {} ms for controller shutdown completion...",
                     (now - start).to_msecs());
          next_log_time = now + zx::msec(timeout_ms);
        }
        zx::nanosleep(zx::deadline_after(zx::msec(1)));
      }
    }

    cc = ControllerConfigReg::Get().ReadFrom(&*mmio_);
    csts = ControllerStatusReg::Get().ReadFrom(&*mmio_);
    if (cc.enabled() || csts.ready()) {
      cc.set_shutdown_notification(ControllerConfigReg::ShutdownNotification::kNone)
          .set_enabled(0)
          .WriteTo(&*mmio_);
      (void)WaitForReset(/*desired_ready_state=*/false, caps_reg, &*mmio_);
    }
  }

  // Configuring MSI/MSI-X in the PCI bus driver also enables bus mastering prior to
  // SetBusMastering(true), so disable bus mastering whenever interrupts were configured as well.
  if (bus_master_enabled_ || irq_mode_ != fuchsia_hardware_pci::InterruptMode::kDisabled) {
    zx_status_t status = SetBusMastering(false);
    if (status != ZX_OK) {
      fdf::error("Failed to disable PCI bus mastering during teardown: {}",
                 zx_status_get_string(status));
    }
    bus_master_enabled_ = false;
  }

  // Controller is disabled (CSTS.RDY=0) and PCI bus mastering has been turned off; unpin and fail
  // all remaining in-flight transactions.
  if (io_queue_) {
    io_queue_->CancelAll([](IoCommand* io_cmd) { io_cmd->Complete(ZX_ERR_PEER_CLOSED); });
  }
  {
    std::lock_guard<std::mutex> lock(admin_lock_);
    if (admin_queue_) {
      admin_queue_->CancelAll(nullptr);
    }
  }

  // Drain pending (not yet submitted) commands outside commands_lock_.
  list_node_t local_pending = LIST_INITIAL_VALUE(local_pending);
  {
    std::lock_guard<std::mutex> lock(commands_lock_);
    list_move(&pending_commands_, &local_pending);
  }
  IoCommand* io_cmd;
  while ((io_cmd = list_remove_head_type(&local_pending, IoCommand, node)) != nullptr) {
    io_cmd->Complete(ZX_ERR_PEER_CLOSED);
  }
}

static zx_status_t CheckMinMaxSize(const std::string& name, size_t our_size, size_t min_size,
                                   size_t max_size) {
  if (our_size < min_size) {
    fdf::error("{} size is too small (ours: {}, min: {}).", name.c_str(), our_size, min_size);
    return ZX_ERR_NOT_SUPPORTED;
  }
  if (our_size > max_size) {
    fdf::error("{} size is too large (ours: {}, max: {}).", name.c_str(), our_size, max_size);
    return ZX_ERR_NOT_SUPPORTED;
  }
  return ZX_OK;
}

static void PopulateVersionInspect(const VersionReg& version_reg, inspect::Node* inspect_node,
                                   inspect::Inspector* inspector) {
  auto version = inspect_node->CreateChild("version");
  version.RecordInt("major", version_reg.major());
  version.RecordInt("minor", version_reg.minor());
  version.RecordInt("tertiary", version_reg.tertiary());
  inspector->emplace(std::move(version));
}

static void PopulateCapabilitiesInspect(const CapabilityReg& caps_reg,
                                        const VersionReg& version_reg, inspect::Node* inspect_node,
                                        inspect::Inspector* inspector) {
  auto caps = inspect_node->CreateChild("capabilities");
  if (version_reg >= VersionReg::FromVer(1, 4, 0)) {
    caps.RecordBool("controller_ready_independent_media_supported",
                    caps_reg.controller_ready_independent_media_supported());
    caps.RecordBool("controller_ready_with_media_supported",
                    caps_reg.controller_ready_with_media_supported());
  }
  caps.RecordBool("subsystem_shutdown_supported", caps_reg.subsystem_shutdown_supported());
  caps.RecordBool("controller_memory_buffer_supported",
                  caps_reg.controller_memory_buffer_supported());
  caps.RecordBool("persistent_memory_region_supported",
                  caps_reg.persistent_memory_region_supported());
  caps.RecordInt("memory_page_size_max_bytes", caps_reg.memory_page_size_max_bytes());
  caps.RecordInt("memory_page_size_min_bytes", caps_reg.memory_page_size_min_bytes());
  caps.RecordInt("controller_power_scope", caps_reg.controller_power_scope());
  caps.RecordBool("boot_partition_support", caps_reg.boot_partition_support());
  caps.RecordBool("no_io_command_set_support", caps_reg.no_io_command_set_support());
  caps.RecordBool("identify_io_command_set_support", caps_reg.identify_io_command_set_support());
  caps.RecordBool("nvm_command_set_support", caps_reg.nvm_command_set_support());
  caps.RecordBool("nvm_subsystem_reset_supported", caps_reg.nvm_subsystem_reset_supported());
  caps.RecordInt("doorbell_stride_bytes", caps_reg.doorbell_stride_bytes());
  // TODO(https://fxbug.dev/42053036): interpret CRTO register if version > 1.4
  caps.RecordInt("ready_timeout_ms", caps_reg.timeout_ms());
  caps.RecordBool("vendor_specific_arbitration_supported",
                  caps_reg.vendor_specific_arbitration_supported());
  caps.RecordBool("weighted_round_robin_arbitration_supported",
                  caps_reg.weighted_round_robin_arbitration_supported());
  caps.RecordBool("contiguous_queues_required", caps_reg.contiguous_queues_required());
  caps.RecordInt("max_queue_entries", caps_reg.max_queue_entries());
  inspector->emplace(std::move(caps));
}

static void PopulateControllerInspect(const IdentifyController& identify,
                                      uint32_t max_data_transfer_bytes,
                                      uint16_t atomic_write_unit_normal,
                                      uint16_t atomic_write_unit_power_fail,
                                      bool volatile_write_cache_present,
                                      bool volatile_write_cache_enabled,
                                      inspect::Node* inspect_node, inspect::Inspector* inspector) {
  auto controller = inspect_node->CreateChild("controller");
  auto model_number = std::string(identify.model_number, sizeof(identify.model_number));
  auto serial_number = std::string(identify.serial_number, sizeof(identify.serial_number));
  auto firmware_rev = std::string(identify.firmware_rev, sizeof(identify.firmware_rev));
  // Some vendors don't pad the strings with spaces (0x20). Null-terminate strings to avoid printing
  // illegal characters.
  model_number = std::string(model_number.c_str());
  serial_number = std::string(serial_number.c_str());
  firmware_rev = std::string(firmware_rev.c_str());
  fdf::info("Model number:  '{}'", model_number.c_str());
  fdf::info("Firmware rev.: '{}'", firmware_rev.c_str());
  controller.RecordString("model_number", model_number);
  controller.RecordString("serial_number", serial_number);
  controller.RecordString("firmware_rev", firmware_rev);
  controller.RecordInt("max_outstanding_commands", identify.max_cmd);
  controller.RecordInt("num_namespaces", identify.num_namespaces);
  controller.RecordInt("max_allowed_namespaces", identify.max_allowed_namespaces);
  controller.RecordBool("sgl_support", identify.sgl_support & 3);
  controller.RecordInt("max_data_transfer_bytes", max_data_transfer_bytes);
  controller.RecordInt("sanitize_caps", identify.sanicap & 3);
  controller.RecordInt("abort_command_limit", identify.acl + 1);
  controller.RecordInt("asynch_event_req_limit", identify.aerl + 1);
  controller.RecordInt("firmware_slots", (identify.frmw >> 1) & 3);
  controller.RecordBool("firmware_reset", !(identify.frmw & (1 << 4)));
  controller.RecordBool("firmware_slot1ro", identify.frmw & 1);
  controller.RecordInt("host_buffer_min_pages", identify.hmmin);
  controller.RecordInt("host_buffer_preferred_pages", identify.hmpre);
  controller.RecordInt("capacity_total", identify.tnvmcap[0]);
  controller.RecordInt("capacity_unalloc", identify.unvmcap[0]);
  controller.RecordBool("volatile_write_cache_present", volatile_write_cache_present);
  controller.RecordBool("volatile_write_cache_enabled", volatile_write_cache_enabled);
  controller.RecordInt("atomic_write_unit_normal_blocks", atomic_write_unit_normal);
  controller.RecordInt("atomic_write_unit_power_fail_blocks", atomic_write_unit_power_fail);
  controller.RecordBool("doorbell_buffer_config", identify.doorbell_buffer_config());
  controller.RecordBool("virtualization_management", identify.virtualization_management());
  controller.RecordBool("nvme_mi_send_recv", identify.nvme_mi_send_recv());
  controller.RecordBool("directive_send_recv", identify.directive_send_recv());
  controller.RecordBool("device_self_test", identify.device_self_test());
  controller.RecordBool("namespace_management", identify.namespace_management());
  controller.RecordBool("firmware_download_commit", identify.firmware_download_commit());
  controller.RecordBool("format_nvm", identify.format_nvm());
  controller.RecordBool("security_send_recv", identify.security_send_recv());
  controller.RecordBool("timestamp", identify.timestamp());
  controller.RecordBool("reservations", identify.reservations());
  controller.RecordBool("save_select_nonzero", identify.save_select_nonzero());
  controller.RecordBool("write_uncorrectable", identify.write_uncorrectable());
  controller.RecordBool("compare", identify.compare());
  inspector->emplace(std::move(controller));
}

zx_status_t Nvme::ConfigureInterrupt() {
  // Explicitly request 2 MSI-X vectors first so Admin CQ (queue 0, vector 0) and IO CQ (queue 1,
  // vector 1) do not share vector 0 (avoiding shared-vector hazards and virtualized NVMe
  // live-migration vector-0 filtering), without letting ConfigureInterruptMode(2) downgrade a
  // 1-vector MSI-X device to 2-vector MSI. Fall back to 1 shared vector (MSI-X, MSI, or legacy)
  // when 2 MSI-X vectors are unavailable.
  zx_status_t status = SetInterruptMode(fuchsia_hardware_pci::InterruptMode::kMsiX, 2);
  if (status == ZX_OK) {
    irq_mode_ = fuchsia_hardware_pci::InterruptMode::kMsiX;
    io_irq_vector_ = 1;
  } else {
    zx::result<fuchsia_hardware_pci::InterruptMode> mode = ConfigureInterruptMode(1);
    if (mode.is_error()) {
      fdf::error("Failed to configure interrupt: {}", mode.status_string());
      return mode.status_value();
    }
    irq_mode_ = *mode;
    io_irq_vector_ = 0;
  }
  fdf::info("Configured interrupt mode {} with IO IRQ vector {}.", static_cast<uint8_t>(irq_mode_),
            io_irq_vector_);
  inspect_node_.RecordUint("interrupt_mode", static_cast<uint8_t>(irq_mode_));
  inspect_node_.RecordUint("io_irq_vector", io_irq_vector_);

  zx::result<zx::interrupt> irq = MapInterrupt(io_irq_vector_);
  if (irq.is_error()) {
    fdf::error("Failed to map interrupt vector {}: {}", io_irq_vector_, irq.status_string());
    return irq.status_value();
  }
  irq_ = std::move(*irq);
  return ZX_OK;
}

zx_status_t Nvme::Init() {
  {
    std::lock_guard<std::mutex> lock(commands_lock_);
    list_initialize(&pending_commands_);
  }

  VersionReg version_reg = VersionReg::Get().ReadFrom(&*mmio_);
  CapabilityReg caps_reg = CapabilityReg::Get().ReadFrom(&*mmio_);

  if (component_inspector_) {
    inspect_node_ = component_inspector_->root().CreateChild("nvme");
    interrupt_count_property_ = inspect_node_.CreateUint("interrupt_count", 0);
    PopulateVersionInspect(version_reg, &inspect_node_, &component_inspector_->inspector());
    PopulateCapabilitiesInspect(caps_reg, version_reg, &inspect_node_,
                                &component_inspector_->inspector());
  }

  const size_t kPageSize = zx_system_get_page_size();
  zx_status_t status =
      CheckMinMaxSize("System page", kPageSize, caps_reg.memory_page_size_min_bytes(),
                      caps_reg.memory_page_size_max_bytes());
  if (status != ZX_OK) {
    return status;
  }

  if (!caps_reg.nvm_command_set_support()) {
    fdf::error("Controller does not support the NVM command set (CAP.CSS).");
    return ZX_ERR_NOT_SUPPORTED;
  }

  // Disable/reset the controller before enabling PCI bus mastering or unmasking interrupts so a
  // controller left active by firmware cannot DMA into reclaimed memory or fire stale interrupts.
  ControllerConfigReg initial_cc = ControllerConfigReg::Get().ReadFrom(&*mmio_);
  ControllerStatusReg initial_csts = ControllerStatusReg::Get().ReadFrom(&*mmio_);
  if (initial_csts.ready() || initial_cc.enabled()) {
    fdf::debug("Controller is already enabled. Resetting it.");
    initial_cc.set_shutdown_notification(ControllerConfigReg::ShutdownNotification::kNone)
        .set_enabled(0)
        .WriteTo(&*mmio_);
  }
  status = WaitForReset(/*desired_ready_state=*/false, caps_reg, &*mmio_);
  if (status != ZX_OK) {
    return status;
  }

  status = ConfigureInterrupt();
  if (status != ZX_OK) {
    return status;
  }

  status = SetBusMastering(true);
  if (status != ZX_OK) {
    fdf::error("Failed to enable bus mastering: {}", zx_status_get_string(status));
    return status;
  }
  bus_master_enabled_ = true;

  // Set up admin submission and completion queues.
  zx::result admin_queue =
      QueuePair::Create(bti_.borrow(), 0, kAdminQueueMaxEntries, caps_reg, *mmio_,
                        /*prealloc_prp=*/false);
  if (admin_queue.is_error()) {
    fdf::error("Failed to set up admin queue: {}", admin_queue.status_string());
    return admin_queue.status_value();
  }
  {
    std::lock_guard<std::mutex> lock(admin_lock_);
    admin_queue_ = std::move(*admin_queue);

    // Configure the admin queue without reading stale hardware values.
    AdminQueueAttributesReg::Get()
        .FromValue(0)
        .set_completion_queue_size(admin_queue_->completion().entry_count() - 1)
        .set_submission_queue_size(admin_queue_->submission().entry_count() - 1)
        .WriteTo(&*mmio_);

    AdminQueueAddressReg::CompletionQueue()
        .FromValue(0)
        .set_addr(admin_queue_->completion().GetDeviceAddress())
        .WriteTo(&*mmio_);
    AdminQueueAddressReg::SubmissionQueue()
        .FromValue(0)
        .set_addr(admin_queue_->submission().GetDeviceAddress())
        .WriteTo(&*mmio_);
  }

  fdf::debug("Enabling controller.");
  ControllerConfigReg::Get()
      .FromValue(0)
      .set_controller_ready_independent_of_media(0)
      // Queue entry sizes are powers of two.
      .set_io_completion_queue_entry_size(__builtin_ctzl(sizeof(Completion)))
      .set_io_submission_queue_entry_size(__builtin_ctzl(sizeof(Submission)))
      .set_arbitration_mechanism(ControllerConfigReg::ArbitrationMechanism::kRoundRobin)
      // We know that page size is always at least 4096 (required by spec), and we check
      // that zx_system_get_page_size is supported by the controller above.
      .set_memory_page_size(__builtin_ctzl(kPageSize) - 12)
      .set_io_command_set(ControllerConfigReg::CommandSet::kNvm)
      .set_enabled(1)
      .WriteTo(&*mmio_);

  status = WaitForReset(/*desired_ready_state=*/true, caps_reg, &*mmio_);
  if (status != ZX_OK) {
    return status;
  }

  // Timeout may have changed, so double check it.
  caps_reg.ReadFrom(&*mmio_);

  // Set up IO submission and completion queues.
  zx::result io_queue =
      QueuePair::Create(bti_.borrow(), 1, caps_reg.max_queue_entries(), caps_reg, *mmio_,
                        /*prealloc_prp=*/true);
  if (io_queue.is_error()) {
    fdf::error("Failed to set up io queue: {}", io_queue.status_string());
    return io_queue.status_value();
  }
  io_queue_ = std::move(*io_queue);
  inspect_node_.RecordInt("io_submission_queue_size", io_queue_->submission().entry_count());
  inspect_node_.RecordInt("io_completion_queue_size", io_queue_->completion().entry_count());

  // TODO: Switch to use dispatcher threads instead.
  // Spin up IRQ thread so we can start issuing admin commands to the device.
  int thrd_status = thrd_create_with_name(
      &irq_thread_, [](void* ctx) { return static_cast<Nvme*>(ctx)->IrqLoop(); }, this,
      "nvme-irq-thread");
  if (thrd_status != thrd_success) {
    status = thrd_status_to_zx_status(thrd_status);
    fdf::error(" Failed to create IRQ thread: {}", zx_status_get_string(status));
    return status;
  }
  irq_thread_started_ = true;

  // TODO: Switch to use dispatcher threads instead.
  // Spin up IO thread so we can start issuing IO commands from namespace(s).
  thrd_status = thrd_create_with_name(
      &io_thread_, [](void* ctx) { return static_cast<Nvme*>(ctx)->IoLoop(); }, this,
      "nvme-io-thread");
  if (thrd_status != thrd_success) {
    status = thrd_status_to_zx_status(thrd_status);
    fdf::error(" Failed to create IO thread: {}", zx_status_get_string(status));
    return status;
  }
  io_thread_started_ = true;

  zx::vmo admin_data;
  status = zx::vmo::create(kPageSize, 0, &admin_data);
  if (status != ZX_OK) {
    fdf::error("Failed to create vmo: {}", zx_status_get_string(status));
    return status;
  }

  fzl::VmoMapper mapper;
  status = mapper.Map(admin_data);
  if (status != ZX_OK) {
    fdf::error("Failed to map vmo: {}", zx_status_get_string(status));
    return status;
  }

  IdentifySubmission identify_controller;
  identify_controller.set_structure(IdentifySubmission::IdentifyCns::kIdentifyController);
  zx::result<Completion> completion = DoAdminCommandSync(identify_controller, admin_data.borrow());
  if (completion.is_error()) {
    fdf::error("Failed to identify controller: {}", completion.status_string());
    return completion.status_value();
  }

  auto identify = static_cast<IdentifyController*>(mapper.start());

  status = CheckMinMaxSize("Submission queue entry", sizeof(Submission),
                           identify->minimum_sq_entry_size(), identify->maximum_sq_entry_size());
  if (status != ZX_OK) {
    return status;
  }
  status = CheckMinMaxSize("Completion queue entry", sizeof(Completion),
                           identify->minimum_cq_entry_size(), identify->maximum_cq_entry_size());
  if (status != ZX_OK) {
    return status;
  }

  if (identify->max_data_transfer == 0) {
    max_data_transfer_bytes_ = 0;
  } else {
    const uint32_t shift =
        12 + static_cast<uint32_t>(caps_reg.memory_page_size_min()) + identify->max_data_transfer;
    if (shift >= 32) {
      max_data_transfer_bytes_ = std::numeric_limits<uint32_t>::max();
    } else {
      max_data_transfer_bytes_ = uint32_t{1} << shift;
    }
  }
  atomic_write_unit_normal_ = identify->atomic_write_unit_normal + 1;
  atomic_write_unit_power_fail_ = identify->atomic_write_unit_power_fail + 1;

  bool volatile_write_cache_present = identify->vwc & 1;
  if (volatile_write_cache_present) {
    // Get 'Volatile Write Cache Enable' feature.
    GetVolatileWriteCacheSubmission get_vwc_enable;
    completion = DoAdminCommandSync(get_vwc_enable);
    if (completion.is_error()) {
      fdf::error("Failed to get 'Volatile Write Cache' feature: {}", completion.status_string());
      return completion.status_value();
    } else {
      const auto& vwc_completion = completion->GetCompletion<GetVolatileWriteCacheCompletion>();
      volatile_write_cache_enabled_ = vwc_completion.get_volatile_write_cache_enabled();
      fdf::debug("Volatile write cache is {}",
                 volatile_write_cache_enabled_ ? "enabled" : "disabled");
    }
  }

  if (component_inspector_) {
    PopulateControllerInspect(*identify, max_data_transfer_bytes_, atomic_write_unit_normal_,
                              atomic_write_unit_power_fail_, volatile_write_cache_present,
                              volatile_write_cache_enabled_, &inspect_node_, &inspect());
  }

  // Set feature (number of queues) to 1 IO submission queue and 1 IO completion queue.
  SetIoQueueCountSubmission set_queue_count;
  set_queue_count.set_num_submission_queues(1).set_num_completion_queues(1);
  completion = DoAdminCommandSync(set_queue_count);
  if (completion.is_error()) {
    fdf::error("Failed to set feature (number of queues): {}", completion.status_string());
    return completion.status_value();
  }
  const auto& ioq_completion = completion->GetCompletion<SetIoQueueCountCompletion>();
  if (ioq_completion.num_submission_queues() < 1) {
    fdf::error("Unexpected IO submission queue count: {}", ioq_completion.num_submission_queues());
    return ZX_ERR_IO;
  }
  if (ioq_completion.num_completion_queues() < 1) {
    fdf::error("Unexpected IO completion queue count: {}", ioq_completion.num_completion_queues());
    return ZX_ERR_IO;
  }

  // Create IO completion queue.
  CreateIoCompletionQueueSubmission create_iocq;
  create_iocq.set_queue_id(io_queue_->completion().id())
      .set_queue_size(io_queue_->completion().entry_count() - 1)
      .set_contiguous(true)
      .set_interrupt_en(true)
      .set_interrupt_vector(static_cast<uint16_t>(io_irq_vector_));
  create_iocq.data_pointer[0] = io_queue_->completion().GetDeviceAddress();
  completion = DoAdminCommandSync(create_iocq);
  if (completion.is_error()) {
    fdf::error("Failed to create IO completion queue: {}", completion.status_string());
    return completion.status_value();
  }

  // Create IO submission queue.
  CreateIoSubmissionQueueSubmission create_iosq;
  create_iosq.set_queue_id(io_queue_->submission().id())
      .set_queue_size(io_queue_->submission().entry_count() - 1)
      .set_completion_queue_id(io_queue_->completion().id())
      .set_contiguous(true);
  create_iosq.data_pointer[0] = io_queue_->submission().GetDeviceAddress();
  completion = DoAdminCommandSync(create_iosq);
  if (completion.is_error()) {
    fdf::error("Failed to create IO submission queue: {}", completion.status_string());
    return completion.status_value();
  }

  // Identify active namespaces.
  IdentifySubmission identify_ns_list;
  identify_ns_list.set_structure(IdentifySubmission::IdentifyCns::kActiveNamespaceList);
  completion = DoAdminCommandSync(identify_ns_list, admin_data.borrow());
  if (completion.is_error()) {
    fdf::error("Failed to identify active namespace list: {}", completion.status_string());
    return completion.status_value();
  }

  // Bind active namespaces.
  auto ns_list = static_cast<IdentifyActiveNamespaces*>(mapper.start());
  for (size_t i = 0; i < std::size(ns_list->nsid) && ns_list->nsid[i] != 0; i++) {
    if (i >= kMaxNamespacesToBind) {
      fdf::warn("Skipping additional namespaces after adding {}.", i);
      break;
    }
    const uint32_t namespace_id = ns_list->nsid[i];
    zx::result<std::unique_ptr<Namespace>> ns = Namespace::Bind(this, namespace_id);
    if (ns.is_error()) {
      fdf::error("Failed to add namespace {}: {}", namespace_id, ns.status_string());
      return ns.status_value();
    }
    namespaces_.push_back(*std::move(ns));
  }

  return ZX_OK;
}

zx_status_t Nvme::SetInterruptMode(fuchsia_hardware_pci::InterruptMode mode,
                                   uint32_t requested_irq_count) {
  return pci_.SetInterruptMode(mode, requested_irq_count);
}

zx::result<fuchsia_hardware_pci::InterruptMode> Nvme::ConfigureInterruptMode(uint32_t irq_count) {
  fuchsia_hardware_pci::InterruptMode mode = fuchsia_hardware_pci::InterruptMode::kDisabled;
  zx_status_t status = pci_.ConfigureInterruptMode(irq_count, &mode);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(mode);
}

zx::result<zx::interrupt> Nvme::MapInterrupt(uint32_t which_irq) {
  zx::interrupt interrupt;
  zx_status_t status = pci_.MapInterrupt(which_irq, &interrupt);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(std::move(interrupt));
}

zx_status_t Nvme::SetBusMastering(bool enabled) {
  if (!pci_.is_valid()) {
    return ZX_OK;
  }
  return pci_.SetBusMastering(enabled);
}

zx::result<> Nvme::InitResources() {
  zx::result<fdf::Pci> pci = fdf::Pci::Connect(incoming_, fdf::Pci::kFragmentName);
  if (pci.is_error()) {
    fdf::error("Failed to connect to PCI: {}", pci.status_string());
    return pci.take_error();
  }

  pci_ = std::move(pci.value());

  std::optional<fdf::MmioBuffer> mmio;
  zx_status_t status = pci_.MapMmio(0u, ZX_CACHE_POLICY_UNCACHED_DEVICE, &mmio);
  if (status != ZX_OK) {
    fdf::error("Failed to map registers: {}", zx_status_get_string(status));
    return zx::error(status);
  }
  mmio_ = std::move(mmio);

  status = pci_.GetBti(0, &bti_);
  if (status != ZX_OK) {
    fdf::error("Failed to get BTI handle: {}", zx_status_get_string(status));
    return zx::error(status);
  }
  return zx::ok();
}

zx::result<> Nvme::Start(fdf::DriverContext context) {
  incoming_ = std::shared_ptr<fdf::Namespace>(context.take_incoming());
  node_name_ = context.node_name();
  node_token_ = context.take_node_token();

  zx::result<> resources = InitResources();
  if (resources.is_error()) {
    return resources.take_error();
  }

  auto [controller_client_end, controller_server_end] =
      fidl::Endpoints<fuchsia_driver_framework::NodeController>::Create();
  auto [node_client_end, node_server_end] =
      fidl::Endpoints<fuchsia_driver_framework::Node>::Create();

  node_controller_.Bind(std::move(controller_client_end));
  root_node_.Bind(std::move(node_client_end));

  fidl::Arena arena;

  const auto args =
      fuchsia_driver_framework::wire::NodeAddArgs::Builder(arena).name(arena, name()).Build();

  auto result = fidl::WireCall(node().borrow())
                    ->AddChild(args, std::move(controller_server_end), std::move(node_server_end));
  if (!result.ok()) {
    fdf::error("Failed to add child: {}", result.status_string());
    return zx::error(result.status());
  }

  auto connect_result = incoming_->Connect<fuchsia_inspect::InspectSink>();
  if (connect_result.is_ok()) {
    component_inspector_.emplace(dispatcher(), inspect::PublishOptions{
                                                   .tree_name = "nvme",
                                                   .client_end = std::move(connect_result.value()),
                                               });
  } else {
    fdf::warn("Failed to connect to InspectSink: {}", connect_result.status_string());
  }
  zx_status_t status = Init();
  if (status != ZX_OK) {
    fdf::error("Driver initialization failed: {}", zx_status_get_string(status));
    PerformTeardown();
    return zx::error(status);
  }

  return zx::ok();
}

}  // namespace nvme
