// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_XHCI_XHCI_INTERRUPTER_H_
#define SRC_DEVICES_USB_DRIVERS_XHCI_XHCI_INTERRUPTER_H_

#include <lib/async/cpp/irq.h>
#include <lib/async/cpp/task.h>
#include <lib/driver/power/cpp/wake-lease.h>
#include <lib/fit/defer.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zx/interrupt.h>

#include <atomic>
#include <memory>
#include <variant>

#include <fbl/auto_lock.h>
#include <fbl/mutex.h>

#include "src/devices/usb/drivers/xhci/xhci-event-ring.h"

namespace usb_xhci {

struct Inspect;  // fwd decl
class InterrupterTestPeer;

// An interrupter that manages an event ring, and handles interrupts.
class Interrupter {
 public:
  Interrupter() = default;

  ~Interrupter() { Stop(); }

  zx_status_t Init(uint16_t interrupter, size_t page_size, fdf::MmioBuffer* buffer,
                   const RuntimeRegisterOffset& offset, uint32_t erst_max,
                   DoorbellOffset doorbell_offset, UsbXhci* hci, HCCPARAMS1 hcc_params_1,
                   uint64_t* dcbaa);

  // NOLINTNEXTLINE(readability-inconsistent-declaration-parameter-name)
  zx_status_t Start(const RuntimeRegisterOffset& offset, fdf::MmioView interrupter_regs);

  void Stop() {
    while (true) {
      RunningState running;
      std::shared_ptr<libsync::Completion> wait_completion;
      std::shared_ptr<libsync::Completion> my_stop_completion;
      {
        fbl::AutoLock _(&dispatcher_lock_);
        active_ = false;
        if (auto* starting = std::get_if<StartingState>(&dispatcher_state_)) {
          starting->stop_requested = true;
          wait_completion = starting->completion;
        } else if (auto* stopping = std::get_if<StoppingState>(&dispatcher_state_)) {
          wait_completion = stopping->completion;
        } else if (auto* current = std::get_if<RunningState>(&dispatcher_state_)) {
          my_stop_completion = std::make_shared<libsync::Completion>();
          running = std::move(*current);
          dispatcher_state_ = StoppingState{.completion = my_stop_completion};
        } else {
          return;
        }
      }
      if (wait_completion) {
        wait_completion->Wait();
        continue;
      }

      struct CancelState {
        std::atomic<zx_status_t> status{ZX_ERR_CANCELED};
        libsync::Completion done;
      };
      auto cancel_state = std::make_shared<CancelState>();
      zx_status_t post_task_status = async::PostTask(
          running.dispatcher.async_dispatcher(),
          [this, cancel_state,
           signal = fit::defer([cancel_state] { cancel_state->done.Signal(); })] {
            cancel_state->status.store(irq_handler_.Cancel(), std::memory_order_release);
            wake_lease_.reset();
          });
      if (post_task_status != ZX_OK) {
        fdf::error("Failed to post the irq handler cancel task: {}",
                   zx_status_get_string(post_task_status));
      } else {
        cancel_state->done.Wait();
        const zx_status_t cancel_status = cancel_state->status.load(std::memory_order_acquire);
        if (cancel_status != ZX_OK && cancel_status != ZX_ERR_NOT_FOUND &&
            cancel_status != ZX_ERR_CANCELED) {
          fdf::error("Failed to cancel the irq handler: {}", zx_status_get_string(cancel_status));
        }
      }

      running.dispatcher.ShutdownAsync();
      running.shutdown_completion->Wait();
      // Destroy wake_lease_ before running.dispatcher so its async task/binding teardown never
      // references a destroyed dispatcher, and destroy running.dispatcher before signaling
      // my_stop_completion so concurrent Stop() callers observe completed teardown.
      wake_lease_.reset();
      running.dispatcher.reset();
      {
        fbl::AutoLock _(&dispatcher_lock_);
        dispatcher_state_ = std::monostate{};
      }
      my_stop_completion->Signal();
      return;
    }
  }

  EventRing& ring() { return event_ring_; }
  bool active() { return active_; }

  // Returns a pointer to the IRQ
  // owned by this interrupter
  zx::interrupt& GetIrq() { return irq_; }

  fpromise::promise<void, zx_status_t> Timeout(zx::time deadline);

 private:
  friend class InterrupterTestPeer;

  struct StartingState {
    std::shared_ptr<libsync::Completion> completion;
    bool stop_requested = false;
  };
  struct RunningState {
    fdf::SynchronizedDispatcher dispatcher;
    std::shared_ptr<libsync::Completion> shutdown_completion;
  };
  struct StoppingState {
    std::shared_ptr<libsync::Completion> completion;
  };

  zx_status_t StartIrqThread();

  fbl::Mutex dispatcher_lock_;
  std::variant<std::monostate, StartingState, RunningState, StoppingState> dispatcher_state_
      __TA_GUARDED(dispatcher_lock_);

  std::atomic_bool active_ = false;
  uint16_t interrupter_;
  zx::interrupt irq_;
  EventRing event_ring_;
  async::Irq irq_handler_;

  std::optional<fdf_power::TimeoutWakeLease> wake_lease_;

  // published inspect data
  inspect::Node inspect_root_;
  inspect::UintProperty total_irqs_;

  inspect::Node wake_events_;
  inspect::UintProperty total_wake_events_;
  inspect::BoolProperty wake_lease_held_;
  inspect::UintProperty wake_lease_last_acquired_timestamp_;
  inspect::UintProperty wake_lease_last_refreshed_timestamp_;

  // Reference to the xHCI core. Since Interrupter is a part of the
  // UsbXhci (always instantiated as a class member), this reference
  // will always be valid for the lifetime of the Interrupter.
  UsbXhci* hci_ = nullptr;
};

}  // namespace usb_xhci

#endif  // SRC_DEVICES_USB_DRIVERS_XHCI_XHCI_INTERRUPTER_H_
