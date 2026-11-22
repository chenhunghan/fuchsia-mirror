// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_NVME_NVME_H_
#define SRC_DEVICES_BLOCK_DRIVERS_NVME_NVME_H_

#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/mmio/cpp/mmio-buffer.h>
#include <lib/driver/pci/cpp/pci.h>
#include <lib/inspect/component/cpp/component.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/sync/completion.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <lib/zx/result.h>
#include <lib/zx/time.h>
#include <threads.h>
#include <zircon/listnode.h>
#include <zircon/types.h>

#include <atomic>
#include <mutex>

#include "src/devices/block/drivers/nvme/commands.h"
#include "src/devices/block/drivers/nvme/io-command.h"
#include "src/devices/block/drivers/nvme/namespace.h"
#include "src/devices/block/drivers/nvme/queue-pair.h"
#include "src/devices/block/drivers/nvme/registers.h"

namespace fake_nvme {
class FakeController;
}

namespace nvme {

class Nvme : public fdf::DriverBase2 {
 public:
  static constexpr char kDriverName[] = "nvme";

  explicit Nvme() : fdf::DriverBase2(kDriverName) {}
  ~Nvme() override;

  zx::result<> Start(fdf::DriverContext context) override;

  void Stop(fdf::StopCompleter completer) override;

  // Perform an admin command synchronously (i.e., blocks for the command to complete or timeout).
  // Returns the command completion.
  zx::result<Completion> DoAdminCommandSync(
      Submission& submission, std::optional<zx::unowned_vmo> admin_data = std::nullopt);

  // Queue an IO command to be performed asynchronously.
  void QueueIoCommand(IoCommand* io_cmd);

  bool has_inspector() const { return component_inspector_.has_value(); }
  inspect::Inspector& inspect() { return component_inspector_->inspector(); }
  inspect::Node& inspect_node() { return inspect_node_; }

  QueuePair* io_queue() const { return io_queue_.get(); }
  uint32_t max_data_transfer_bytes() const { return max_data_transfer_bytes_; }
  bool volatile_write_cache_enabled() const { return volatile_write_cache_enabled_; }
  uint16_t atomic_write_unit_normal() const { return atomic_write_unit_normal_; }
  uint16_t atomic_write_unit_power_fail() const { return atomic_write_unit_power_fail_; }
  fuchsia_hardware_pci::InterruptMode irq_mode() const { return irq_mode_; }
  uint32_t io_irq_vector() const { return io_irq_vector_; }

  std::vector<std::unique_ptr<nvme::Namespace>>& namespaces() { return namespaces_; }

  // Called by children device of this controller for invoking AddChild() or instantiating
  // compat::DeviceServer.
  fidl::WireSyncClient<fuchsia_driver_framework::Node>& root_node() { return root_node_; }
  std::string_view driver_name() const { return name(); }
  const std::shared_ptr<fdf::Namespace>& driver_incoming() const { return incoming_; }
  std::shared_ptr<fdf::OutgoingDirectory>& driver_outgoing() { return outgoing(); }
  const std::optional<std::string>& driver_node_name() const { return node_name_; }

  zx::event node_token() const {
    zx::event copy;
    if (node_token_.is_valid()) {
      node_token_.duplicate(ZX_RIGHT_SAME_RIGHTS, &copy);
    }
    return copy;
  }

 protected:
  // Initializes MMIO and BTI resources prior to controller reset. Override to inject dependencies
  // for unit testing.
  virtual zx::result<> InitResources();
  virtual zx_status_t SetInterruptMode(fuchsia_hardware_pci::InterruptMode mode,
                                       uint32_t requested_irq_count);
  virtual zx::result<fuchsia_hardware_pci::InterruptMode> ConfigureInterruptMode(
      uint32_t irq_count);
  virtual zx::result<zx::interrupt> MapInterrupt(uint32_t which_irq);
  virtual zx_status_t SetBusMastering(bool enabled);

  // Initialized on the driver dispatcher in InitResources()/Init() before irq_thread_ and
  // io_thread_ are spawned; read-only while worker threads run, and torn down in PerformTeardown()
  // (irq_.destroy() cancels IrqLoop(), then worker threads are joined before hardware reset).
  fdf::Pci pci_;
  std::optional<fdf::MmioBuffer> mmio_;
  fuchsia_hardware_pci::InterruptMode irq_mode_ = fuchsia_hardware_pci::InterruptMode::kDisabled;
  uint32_t io_irq_vector_ = 0;
  zx::interrupt irq_;
  zx::bti bti_;

 private:
  friend class fake_nvme::FakeController;

  int IrqLoop();
  int IoLoop();

  // Main driver initialization.
  zx_status_t Init();
  zx_status_t ConfigureInterrupt();

  // Process pending IO commands. Called in the IoLoop().
  void ProcessIoSubmissions();
  // Process pending IO completions. Called in the IoLoop() (or PerformTeardown() after joining
  // threads).
  void ProcessIoCompletions();
  void PerformTeardown();

  // Thread-safety invariants across the driver dispatcher, irq_thread_, io_thread_, and
  // BlockServer worker threads:
  //  - Driver-dispatcher / init-once state (incoming_, component_inspector_, node_name_,
  //    node_token_, inspect_node_, max_data_transfer_bytes_, volatile_write_cache_enabled_,
  //    atomic_write_unit_*, namespaces_, FIDL node clients, and lifecycle flags irq_thread_*,
  //    io_thread_*, bus_master_enabled_, torn_down_): mutated only on the driver dispatcher in
  //    Start()/Init()/Stop()/PerformTeardown() (or ~Nvme()); capability fields and node_token_ are
  //    immutable once namespaces start serving requests.
  //  - Inspect counters (interrupt_count_property_): created in Init() before worker threads start;
  //    inspect::UintProperty mutations are internally thread-safe.
  //  - commands_lock_: guards pending_commands_ between BlockServer threads (QueueIoCommand()),
  //    io_thread_ (ProcessIoSubmissions()), and PerformTeardown().
  //  - admin_lock_: serializes DoAdminCommandSync() callers and PerformTeardown() access to
  //    admin_queue_.
  //  - legacy_irq_lock_: guards legacy_irq_masked_ and legacy_irq_mask_gen_ to serialize non-MSI-X
  //    INTMS (IrqLoop()) and INTMC (IoLoop()) updates.
  //  - io_queue_: created in Init() before io_thread_ starts, accessed exclusively on io_thread_
  //    while running, and cancelled in PerformTeardown() only after thrd_join(io_thread_).
  //  - io_signal_: thread-safe sync_completion_t signaled by QueueIoCommand(), IrqLoop(), and
  //    PerformTeardown(), and waited/reset on io_thread_.
  //  - driver_shutdown_: stored true with memory_order_release in PerformTeardown() while holding
  //    both admin_lock_ and commands_lock_. QueueIoCommand() and DoAdminCommandSync() check it
  //    under their respective locks so no new work is accepted after teardown begins, while
  //    IoLoop() loads it locklessly with memory_order_acquire to exit without taking a mutex on
  //    every wakeup.
  std::shared_ptr<fdf::Namespace> incoming_;
  std::optional<inspect::ComponentInspector> component_inspector_;
  std::optional<std::string> node_name_;
  zx::event node_token_;
  inspect::Node inspect_node_;
  inspect::UintProperty interrupt_count_property_;

  std::mutex commands_lock_;
  // The pending list consists of commands that have been received via QueueIoCommand() and are
  // waiting for IO to start.
  list_node_t pending_commands_ TA_GUARDED(commands_lock_) = LIST_INITIAL_VALUE(pending_commands_);

  // Admin submission and completion queues.
  std::mutex admin_lock_;  // Used to serialize admin transactions.
  std::unique_ptr<QueuePair> admin_queue_ TA_GUARDED(admin_lock_);

  // IO submission and completion queues.
  std::unique_ptr<QueuePair> io_queue_;
  // Notifies IoThread() that it has work to do. Signaled from QueueIoCommand() or the IRQ handler.
  sync_completion_t io_signal_;
  std::mutex legacy_irq_lock_;
  bool legacy_irq_masked_ TA_GUARDED(legacy_irq_lock_) = false;
  uint64_t legacy_irq_mask_gen_ TA_GUARDED(legacy_irq_lock_) = 0;

  thrd_t irq_thread_;
  thrd_t io_thread_;
  bool irq_thread_started_ = false;
  bool io_thread_started_ = false;
  bool bus_master_enabled_ = false;
  bool torn_down_ = false;

  uint32_t max_data_transfer_bytes_ = 0;
  // This flag indicates whether the volatile write cache of the device is enabled. It can only be
  // enabled if the volatile write cache is supported.
  bool volatile_write_cache_enabled_ = false;
  std::atomic<bool> driver_shutdown_ = false;

  uint16_t atomic_write_unit_normal_ = 0;
  uint16_t atomic_write_unit_power_fail_ = 0;

  std::vector<std::unique_ptr<nvme::Namespace>> namespaces_;

  fidl::WireSyncClient<fuchsia_driver_framework::Node> parent_node_;
  fidl::WireSyncClient<fuchsia_driver_framework::Node> root_node_;
  fidl::WireSyncClient<fuchsia_driver_framework::NodeController> node_controller_;
};

}  // namespace nvme

#endif  // SRC_DEVICES_BLOCK_DRIVERS_NVME_NVME_H_
