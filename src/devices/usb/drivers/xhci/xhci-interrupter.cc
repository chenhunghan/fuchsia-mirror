// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/xhci/xhci-interrupter.h"

#include <lib/async/cpp/task.h>

#include "src/devices/usb/drivers/xhci/usb-xhci.h"

namespace usb_xhci {

zx_status_t Interrupter::Init(uint16_t interrupter, size_t page_size, fdf::MmioBuffer* buffer,
                              const RuntimeRegisterOffset& offset, uint32_t erst_max,
                              DoorbellOffset doorbell_offset, UsbXhci* hci, HCCPARAMS1 hcc_params_1,
                              uint64_t* dcbaa) {
  if (active_) {
    // Already active;
    return ZX_OK;
  }

  hci_ = hci;
  interrupter_ = interrupter;

  // Create our inspect node and start to add our properties.
  char name[64];
  snprintf(name, sizeof(name), "Interrupter %hu", interrupter_);
  inspect_root_ = hci_->inspect_root_node().CreateChild(name);
  total_irqs_ = inspect_root_.CreateUint("Total IRQs", 0);
  wake_events_ = inspect_root_.CreateChild("Wake Events");

  return event_ring_.Init(page_size, hci_->bti(), buffer, hci->Is32BitController(), erst_max,
                          ERSTSZ::Get(offset, interrupter_).ReadFrom(buffer),
                          ERDP::Get(offset, interrupter_).ReadFrom(buffer),
                          IMAN::Get(offset, interrupter_).FromValue(0), hci_->CapLength(),
                          HCSPARAMS1::Get().ReadFrom(buffer), hci_->GetCommandRing(),
                          doorbell_offset, hci, hcc_params_1, dcbaa, interrupter_, &inspect_root_);
}

zx_status_t Interrupter::Start(const RuntimeRegisterOffset& offset, fdf::MmioView mmio_view) {
  auto start_completion = std::make_shared<libsync::Completion>();
  {
    fbl::AutoLock _(&dispatcher_lock_);
    if (active_) {
      // Already active;
      return ZX_OK;
    }
    if (!std::holds_alternative<std::monostate>(dispatcher_state_)) {
      if (std::holds_alternative<RunningState>(dispatcher_state_)) {
        fdf::warn(
            "Interrupter {} Start() called while a failed dispatcher is still allocated; "
            "call Stop() before retrying",
            interrupter_);
      }
      return ZX_ERR_BAD_STATE;
    }
    if (!event_ring_.erdp_phys()) {
      return ZX_ERR_BAD_STATE;
    }
    dispatcher_state_ = StartingState{.completion = start_completion};
  }

  bool start_committed = false;
  fdf::SynchronizedDispatcher unused_dispatcher;
  auto shutdown_completion = std::make_shared<libsync::Completion>();
  auto cleanup_start = fit::defer([&] {
    if (unused_dispatcher.get()) {
      unused_dispatcher.ShutdownAsync();
      shutdown_completion->Wait();
      unused_dispatcher.reset();
    }
    if (!start_committed) {
      fbl::AutoLock _(&dispatcher_lock_);
      if (std::holds_alternative<StartingState>(dispatcher_state_)) {
        dispatcher_state_ = std::monostate{};
      }
    }
    start_completion->Signal();
  });

  ERDP erdp = ERDP::Get(offset, interrupter_).ReadFrom(&mmio_view);
  erdp.set_reg_value(event_ring_.erdp_phys());
  erdp.WriteTo(&mmio_view);
  ERSTBA ba = ERSTBA::Get(offset, interrupter_).ReadFrom(&mmio_view);
  // This enables the interrupter
  ba.set_Pointer(event_ring_.erst()).WriteTo(&mmio_view);
  // TODO(https://fxbug.dev/42105800): See https://fxbug.dev/42105800.  Get rid of this.  For now we
  // need thread priorities so that realtime transactions use the completer which ends up getting
  // realtime latency guarantees.
  auto dispatcher_result = fdf::SynchronizedDispatcher::Create(
      {}, "xhci-interrupter",
      [shutdown_completion](fdf_dispatcher_t*) { shutdown_completion->Signal(); },
      "fuchsia.devices.usb.drivers.xhci.interrupter");
  if (dispatcher_result.is_error()) {
    fdf::error("Failed to create new dispatcher {}", dispatcher_result);
    return dispatcher_result.error_value();
  }
  {
    fbl::AutoLock _(&dispatcher_lock_);
    auto* starting = std::get_if<StartingState>(&dispatcher_state_);
    if (!starting || starting->stop_requested) {
      unused_dispatcher = std::move(*dispatcher_result);
      return ZX_ERR_BAD_STATE;
    }
    zx_status_t status =
        async::PostTask(dispatcher_result->async_dispatcher(), [this, offset, mmio_view] mutable {
          if (zx_status_t irq_status = StartIrqThread(); irq_status != ZX_OK) {
            irq_handler_.Cancel();
            IMAN::Get(offset, interrupter_).FromValue(0).set_IE(0).WriteTo(&mmio_view);
            bool still_running = false;
            {
              fbl::AutoLock _(&dispatcher_lock_);
              active_ = false;
              still_running = std::holds_alternative<RunningState>(dispatcher_state_);
            }
            if (still_running) {
              fdf::warn(
                  "Interrupter {} StartIrqThread() failed ({}); disabling interrupts until Stop()",
                  interrupter_, zx_status_get_string(irq_status));
            }
          }
        });
    if (status != ZX_OK) {
      fdf::error("Failed to post StartIrqThread: {}", zx_status_get_string(status));
      unused_dispatcher = std::move(*dispatcher_result);
      return status;
    }
    // Enable IMAN.IE only after dispatcher creation and StartIrqThread() posting succeed so a
    // failed Start() never leaves hardware interrupts enabled. StartIrqThread() acquires
    // dispatcher_lock_ before enabling the IRQ handler or Ring0Bringup(), so IMAN.IE is
    // guaranteed visible before either runs.
    IMAN::Get(offset, interrupter_).FromValue(0).set_IE(1).WriteTo(&mmio_view);
    dispatcher_state_ = RunningState{
        .dispatcher = std::move(*dispatcher_result),
        .shutdown_completion = shutdown_completion,
    };
    active_ = true;
    start_committed = true;
  }
  return ZX_OK;
}

fpromise::promise<void, zx_status_t> Interrupter::Timeout(zx::time deadline) {
  fpromise::bridge<void, zx_status_t> bridge;
  zx_status_t status = ZX_ERR_IO_NOT_PRESENT;
  {
    fbl::AutoLock _(&dispatcher_lock_);
    if (auto* running = std::get_if<RunningState>(&dispatcher_state_); active_ && running) {
      status = async::PostTaskForTime(
          running->dispatcher.async_dispatcher(),
          [completer = std::move(bridge.completer), this] mutable {
            if (!active_) {
              return;
            }
            completer.complete_ok();
            hci_->RunUntilIdle(interrupter_);
          },
          deadline);
    }
  }
  if (status != ZX_OK) {
    return fpromise::make_error_promise<zx_status_t>(
        status == ZX_ERR_BAD_STATE ? ZX_ERR_IO_NOT_PRESENT : status);
  }
  // Resolve with ZX_ERR_IO_NOT_PRESENT if Stop() cancels the pending task and abandons
  // bridge.completer (otherwise .promise() would abandon downstream continuations without
  // resuming them).
  return bridge.consumer.promise_or(fpromise::error(ZX_ERR_IO_NOT_PRESENT)).box();
}

zx_status_t Interrupter::StartIrqThread() {
  async_dispatcher_t* async_dispatcher = nullptr;
  {
    fbl::AutoLock _(&dispatcher_lock_);
    auto* running = std::get_if<RunningState>(&dispatcher_state_);
    if (!active_ || !running) {
      return ZX_ERR_BAD_STATE;
    }
    async_dispatcher = running->dispatcher.async_dispatcher();
  }
  if (!irq_.is_valid()) {
    fdf::error("Failed to start interrupter {}: invalid IRQ handle", interrupter_);
    return ZX_ERR_BAD_HANDLE;
  }
  irq_handler_.set_object(irq_.get());
  irq_handler_.set_handler([&](async_dispatcher_t* dispatcher, async::Irq* irq, zx_status_t status,
                               const zx_packet_interrupt_t* interrupt) {
    if (!irq_.is_valid()) {
      return;
    }
    if (status != ZX_OK) {
      return;
    }

    // TODO(b/362759606): For now, release the lease after a timeout. In the future, this should be
    // passed up the USB stack in "baton-passing" manner.
    const zx::duration kLeaseTimeout = zx::msec(500);
    wake_lease_->HandleInterrupt(kLeaseTimeout);

    if (event_ring_.HandleIRQ() != ZX_OK) {
      fdf::error("Error handling IRQ. Exiting async loop.");
      return;
    }

    // Acknowledge the interrupt after HandleIRQ() has drained the event ring and cleared
    // IMAN.IP/ERDP.EHB so level-triggered interrupts do not immediately re-assert while
    // the event ring is still asserted.
    total_irqs_.Add(1);
    irq_.ack();
  });
  if (zx_status_t begin_status = irq_handler_.Begin(async_dispatcher); begin_status != ZX_OK) {
    fdf::error("Failed to begin irq handler: {}", zx_status_get_string(begin_status));
    return begin_status;
  }
  if (!interrupter_) {
    // Note: We need to run the ring 0 bringup after
    // initializing interrupts, since Qemu initialization
    // code assumes that interrupts are active and simulates
    // a port status changed event.
    if (event_ring_.Ring0Bringup() != ZX_OK) {
      fdf::error("Failed to bring up ring 0");
      return ZX_ERR_INTERNAL;
    }
  }
  // Emplace wake_lease_ only after irq_handler_.Begin() and Ring0Bringup() succeed so a failed
  // start does not connect to ActivityGovernor. Because StartIrqThread() runs on the
  // synchronized async_dispatcher, the IRQ handler cannot fire until this function returns.
  wake_lease_.emplace(async_dispatcher, "xhci-wake", hci_->ConnectActivityGovernor(),
                      &wake_events_);
  return ZX_OK;
}

}  // namespace usb_xhci
