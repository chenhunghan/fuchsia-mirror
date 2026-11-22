// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/xhci/usb-xhci.h"

#include <fidl/fuchsia.hardware.usb.descriptor/cpp/fidl.h>
#include <lib/ddk/metadata.h>
#include <lib/ddk/platform-defs.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/pci/cpp/pci.h>
#include <lib/fit/defer.h>
#include <lib/fpromise/promise.h>
#include <lib/memory_barriers/memory_barriers.h>
#include <lib/trace/event.h>
#include <lib/zx/bti.h>
#include <lib/zx/clock.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/time.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <threads.h>
#include <unistd.h>
#include <zircon/errors.h>
#include <zircon/status.h>
#include <zircon/syscalls-next.h>
#include <zircon/syscalls.h>
#include <zircon/types.h>

#include <algorithm>
#include <cstdint>
#include <memory>
#include <span>
#include <string>
#include <utility>

#include <fbl/alloc_checker.h>
#include <usb/usb-request.h>

#include "src/devices/usb/drivers/xhci/registers.h"
#include "src/devices/usb/drivers/xhci/xhci-context.h"
#include "src/devices/usb/drivers/xhci/xhci-enumeration.h"

namespace usb_xhci {
namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace {

uint32_t kMicroframesPerFrame = 8;  // Per 3.2 spec, 1 frame is 1 ms, 1 microframe is 125 us.

// Obtains the slot index for a specified endpoint.
uint32_t Log2(uint32_t value) { return 31 - __builtin_clz(value); }

uint32_t RoundUp(uint32_t dividend, uint32_t divisor) { return 1 + (dividend - 1) / divisor; }

// Computes the interval value for a specified endpoint.
int ComputeInterval(const usb_endpoint_descriptor_t* ep, usb_speed_t speed) {
  uint8_t interval = ep->b_interval;
  if (usb_ep_is_ctrl(ep) || usb_ep_is_bulk(ep)) {
    if (speed == fdescriptor::UsbSpeed::kHigh) {
      interval = std::clamp(interval, static_cast<uint8_t>(1), static_cast<uint8_t>(16));
      return Log2(interval);
    } else {
      return 0;
    }
  }

  // now we deal with interrupt and isochronous endpoints
  // first make sure bInterval is in legal range
  // See table 6-12 in xHCI specification section 6.2.3.6
  if (usb_ep_is_int(ep) &&
      (speed == fdescriptor::UsbSpeed::kLow || speed == fdescriptor::UsbSpeed::kFull)) {
    interval = static_cast<uint8_t>(std::clamp(static_cast<int>(interval), 1, 255));
  } else {
    interval = static_cast<uint8_t>(std::clamp(static_cast<int>(interval), 1, 16));
  }

  switch (static_cast<fdescriptor::UsbSpeed>(speed)) {
    case fdescriptor::UsbSpeed::kLow:
      return Log2(interval) + 3;  // + 3 to convert 125us to 1ms
    case fdescriptor::UsbSpeed::kFull:
      if (usb_ep_is_isoch(ep)) {
        return (interval - 1) + 3;
      } else {
        return Log2(interval) + 3;
      }
    case fdescriptor::UsbSpeed::kSuper:
    case fdescriptor::UsbSpeed::kHigh:
      return interval - 1;
    default:
      return 0;
  }
}

}  // namespace

bool UsbXhci::Running() const { return running_; }

void UsbXhci::RingDoorbell(uint8_t slot, uint8_t target) {
  DOORBELL::Get(doorbell_offset_, slot).FromValue(0).set_Target(target).WriteTo(&mmio_.value());
}

uint16_t UsbXhci::InterrupterMapping() {
  // No inactive interrupters. Find one with least pressure.
  uint16_t idx = 0;
  size_t min_pressure = interrupter(0).ring().GetPressure();
  for (uint16_t i = 0; i < interrupters_.size(); i++) {
    if (!interrupter(i).active()) {
      continue;
    }
    size_t pressure = interrupter(i).ring().GetPressure();
    if (min_pressure < pressure) {
      idx = i;
      min_pressure = pressure;
    }
  }
  return idx;
}

fpromise::promise<void, zx_status_t> UsbXhci::Timeout(uint16_t target_interrupter,
                                                      zx::time deadline) {
  return interrupter(target_interrupter).Timeout(deadline);
}

fpromise::promise<void, zx_status_t> UsbXhci::DisableSlotCommand(uint32_t slot_id) {
  auto state = slot_id > 0 ? GetDeviceState(slot_id - 1) : nullptr;
  if (!state) {
    fdf::error("{} expects that slot is in use. state should not be nullptr", __func__);
    // TODO(https://fxbug.dev/42067447): Use and test appropriate result status.
    return fpromise::make_error_promise<zx_status_t>(ZX_OK);
  }
  return DisableSlotCommand(*state, state);
}

std::vector<fbl::RefPtr<DeviceState>> UsbXhci::CollectDisconnectSubtree(
    fbl::RefPtr<DeviceState> root) {
  std::vector<fbl::RefPtr<DeviceState>> targets;
  std::vector<bool> visited(max_slots_ + 1, false);
  targets.push_back(root);
  if (root->device_id() <= max_slots_) {
    visited[root->device_id()] = true;
  }

  // The vector is modified in the loop, avoid ranged-for loop here.
  size_t idx = 0;
  while (idx < targets.size()) {
    fbl::RefPtr<DeviceState> cur = targets[idx++];
    std::vector<uint8_t> child_ids;
    {
      fbl::AutoLock _(&cur->transaction_lock());
      cur->Disconnect();
      if (auto& hub = cur->GetHubLocked(); hub.has_value()) {
        for (std::optional<uint8_t>& entry : hub->port_to_device) {
          std::optional<uint8_t> child_id_opt = std::exchange(entry, std::nullopt);
          if (child_id_opt.has_value() && IsValidDeviceId(*child_id_opt) &&
              !visited[*child_id_opt]) {
            visited[*child_id_opt] = true;
            child_ids.push_back(*child_id_opt);
          }
        }
      }
    }
    for (uint8_t child_id : child_ids) {
      if (fbl::RefPtr<DeviceState> child = GetDeviceState(child_id)) {
        targets.push_back(std::move(child));
      }
    }
  }
  return targets;
}

void UsbXhci::DrainAndResetDeviceOnInterrupter(const fbl::RefPtr<DeviceState>& state) {
  for (uint8_t i = 0; i < kMaxEndpoints; ++i) {
    fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> trbs;
    Endpoint* ep = nullptr;
    {
      fbl::AutoLock _(&state->transaction_lock());
      ep = state->TryGetEndpoint(i);
      if (!ep) {
        continue;
      }
      trbs = ep->transfer_ring().TakePendingTRBs();
    }
    for (auto& trb : trbs) {
      if (trb.request.has_value()) {
        ep->RequestComplete(ZX_ERR_IO_NOT_PRESENT, 0, std::move(*trb.request));
      }
    }
  }
  fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> trbs;
  Endpoint* ep0 = nullptr;
  {
    fbl::AutoLock _(&state->transaction_lock());
    ep0 = &state->GetEndpoint();
    trbs = ep0->transfer_ring().TakePendingTRBs();
  }
  for (auto& trb : trbs) {
    if (trb.request.has_value()) {
      ep0->RequestComplete(ZX_ERR_IO_NOT_PRESENT, 0, std::move(*trb.request));
    }
  }
  state->ResetHci();
}

bool UsbXhci::DrainDisabledSubtree(
    const std::vector<fbl::RefPtr<DeviceState>>& targets,
    const fpromise::result<std::vector<fpromise::result<void, zx_status_t>>>& results,
    std::string_view context, std::vector<fbl::RefPtr<DeviceState>>* removable_downstream,
    zx_status_t* root_error) {
  if (results.is_ok()) {
    for (const auto& r : results.value()) {
      if (r.is_error() && r.error() != ZX_ERR_IO_NOT_PRESENT && r.error() != ZX_OK) {
        fdf::warn("DisableSlotCommand failed during {}: {}", context,
                  zx_status_get_string(r.error()));
      }
    }
  }
  bool root_drained = false;
  *root_error = ZX_OK;
  for (size_t i = 0; i < targets.size(); ++i) {
    const bool slot_disabled = results.is_ok() && i < results.value().size() &&
                               (results.value()[i].is_ok() || results.value()[i].error() == ZX_OK);
    if (IsControllerHalted() || slot_disabled) {
      DrainAndResetDeviceOnInterrupter(targets[i]);
      ClearDeviceStateIfMatches(targets[i]->device_id(), targets[i].get());
      if (i == 0) {
        root_drained = true;
      } else {
        removable_downstream->push_back(targets[i]);
      }
    } else {
      const zx_status_t err =
          (results.is_ok() && i < results.value().size() && results.value()[i].is_error())
              ? results.value()[i].error()
              : ZX_ERR_IO;
      fdf::error(
          "Skipping drain and bus RemoveDevice for device {} during {}: controller is still "
          "running after DisableSlotCommand failed ({})",
          targets[i]->device_id(), context, zx_status_get_string(err));
      if (i == 0) {
        *root_error = err;
      }
    }
  }
  if (targets.empty()) {
    *root_error = ZX_ERR_IO_NOT_PRESENT;
  }
  return root_drained;
}

zx_status_t UsbXhci::RemoveSubtreeFromBusOnDdkDispatcher(
    const fbl::RefPtr<DeviceState>& root,
    const std::vector<fbl::RefPtr<DeviceState>>& removable_downstream, zx_status_t root_error) {
  if (!running_ || !SnapshotBusClient().is_valid()) {
    return ZX_OK;
  }
  auto remove_if_added = [this](const fbl::RefPtr<DeviceState>& state) -> zx_status_t {
    bool added = false;
    {
      fbl::AutoLock _(&state->transaction_lock());
      added = state->TakeBusDeviceAddedForRemovalLocked();
    }
    if (!added || !running_) {
      return ZX_OK;
    }
    ddk::UsbBusInterfaceProtocolClient bus = SnapshotBusClient();
    if (!bus.is_valid()) {
      return ZX_OK;
    }
    return bus.RemoveDevice(state->device_id());
  };
  for (const fbl::RefPtr<DeviceState>& t : removable_downstream) {
    zx_status_t status = remove_if_added(t);
    if (status != ZX_OK) {
      fdf::error("Failed to remove downstream device {}: {}", t->device_id(),
                 zx_status_get_string(status));
    }
  }
  if (!root) {
    return root_error;
  }
  return remove_if_added(root);
}

fpromise::promise<void, zx_status_t> UsbXhci::DisableSlotCommand(
    DeviceState& state, fbl::RefPtr<DeviceState> keep_alive) {
  uint8_t slot = 0;
  uint8_t port = 0;
  bool connected_to_hub = false;
  {
    fbl::AutoLock lock(&state.transaction_lock());
    if (!state.IsValid()) {
      fdf::error("DisableSlotCommand state is not valid");
      state.Disconnect();
      state.slot_disabled_.Signal();
      // TODO(https://fxbug.dev/42067447): Use and test appropriate result status.
      return fpromise::make_error_promise<zx_status_t>(ZX_OK);
    }
    state.Disconnect();
    if (state.ExchangeSlotDisableStarted()) {
      // A Disable Slot command has already been issued for this slot.
      if (std::optional<zx_status_t> status = state.SlotDisableStatusLocked(); status.has_value()) {
        if (*status != ZX_OK) {
          return fpromise::make_error_promise<zx_status_t>(*status);
        }
        return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
      }
      // Per Intel xHCI 1.2 Specification Sections 4.6.4 ("Disable Slot") and 6.1 ("Device
      // Context Base Address Array",
      // https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf),
      // concurrent callers must wait for the in-flight Disable Slot command to complete before
      // unpinning transfer rings or releasing Device Context structures referenced by the DCBAA.
      // Even during shutdown (!running_), waiters stay queued until the initiator completes after
      // the controller halts and pending commands are aborted so dcbaa_[slot] and DMA buffers are
      // never released while the xHC may still access them.
      if (!keep_alive) {
        return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO_NOT_PRESENT);
      }
      fpromise::bridge<void, zx_status_t> bridge;
      state.AddSlotDisableWaiterLocked(std::move(bridge.completer));
      lock.release();
      return bridge.consumer.promise()
          .then([keep_alive = std::move(keep_alive)](fpromise::result<void, zx_status_t>& result)
                    -> fpromise::result<void, zx_status_t> { return result; })
          .box();
    }
    slot = state.GetSlot();
    port = state.GetPort();
    // A hub attached directly to a root hub port also has HubInfo, but it still owns the root
    // port's slot. Only devices downstream of an external hub have a non-zero parent port.
    connected_to_hub =
        state.GetHubLocked().has_value() && state.GetHubLocked()->parent_port_number != 0;
  }

  // When keep_alive is null (only from DeviceState::~DeviceState(), when refcount is already 0),
  // asynchronous cleanup must not capture or dereference &state because RunSynchronously() may
  // return early if the controller halts while an interrupter executor still owns the task.
  auto signal = fit::defer([keep_alive] {
    if (keep_alive) {
      keep_alive->CompleteSlotDisable(ZX_ERR_IO_NOT_PRESENT);
    }
  });

  if (!connected_to_hub && port > 0 && port <= port_state_.size() &&
      GetPortState(port).slot_id == slot) {
    GetPortState(port).slot_id = 0;
  }
  if (!running_) {
    if (dcbaa_) {
      dcbaa_[slot] = 0;
    }
    state.CompleteSlotDisable(ZX_ERR_IO_NOT_PRESENT);
    signal.cancel();
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO_NOT_PRESENT);
  }

  DisableSlot cmd;
  cmd.set_slot(slot);
  auto context = command_ring_.AllocateContext();
  if (!context) {
    state.CompleteSlotDisable(ZX_ERR_BAD_STATE);
    signal.cancel();
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_BAD_STATE);
  }

  return SubmitCommand(cmd, std::move(context))
      .then([this, keep_alive = std::move(keep_alive), slot,
             signal = std::move(signal)](fpromise::result<TRB*, zx_status_t>& result) mutable
                -> fpromise::result<void, zx_status_t> {
        zx_status_t status = ZX_OK;
        if (result.is_error()) {
          status = result.error();
        } else {
          TRB* trb = result.value();
          auto* completion_event = static_cast<CommandCompletionEvent*>(trb);
          if (completion_event->CompletionCode() == CommandCompletionEvent::Success ||
              completion_event->CompletionCode() == CommandCompletionEvent::SlotNotEnabledError) {
            if (dcbaa_) {
              dcbaa_[slot] = 0;
            }
          } else {
            status = ZX_ERR_IO;
          }
        }
        if (keep_alive) {
          keep_alive->CompleteSlotDisable(status);
        }
        signal.cancel();
        if (status != ZX_OK) {
          return fpromise::error(status);
        }
        return fpromise::ok();
      })
      .box();
}

TRBPromise UsbXhci::EnableSlotCommand() {
  TRB trb;
  Control::Get().FromValue(0).set_Type(Control::EnableSlot).ToTrb(&trb);
  auto context = command_ring_.AllocateContext();
  return SubmitCommand(trb, std::move(context));
}

fpromise::promise<OwnedRequest, void> UsbXhci::UsbHciRequestQueue(OwnedRequest usb_request) {
  fpromise::bridge<OwnedRequest, void> bridge;
  usb_request_complete_callback_t completion;
  completion.callback = [](void* ctx, usb_request_t* req) {
    auto completer = static_cast<fpromise::completer<OwnedRequest, void>*>(ctx);
    completer->complete_ok(OwnedRequest(req, sizeof(usb_request_t)));
    delete completer;
  };
  completion.ctx = new fpromise::completer<OwnedRequest, void>(std::move(bridge.completer));
  UsbHciRequestQueue(usb_request.take(), &completion);
  return bridge.consumer.promise().box();
}

TRBPromise UsbXhci::AddressDeviceCommand(uint8_t slot_id, uint8_t port_id) {
  if (!IsValidSlotId(slot_id)) {
    return fpromise::make_error_promise(ZX_ERR_INVALID_ARGS);
  }
  fbl::RefPtr<DeviceState> state = GetDeviceState(SlotIdToDeviceId(slot_id));
  if (!state) {
    fdf::error("{} expects that slot is in use. state should not be nullptr", __func__);
    return fpromise::make_error_promise(ZX_ERR_IO_NOT_PRESENT);
  }
  usb_xhci::AddressDeviceStruct cmd;
  {
    fbl::AutoLock _(&state->transaction_lock());
    if (state->IsDisconnecting() || !state->GetInputContext()) {
      return fpromise::make_error_promise(ZX_ERR_IO_NOT_PRESENT);
    }
    // Per Intel xHCI 1.2 Specification Sections 4.3.4, 4.6.5, 6.2.5.1, and 6.4.3.4
    // (https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf),
    // Address Device requires Drop Context Flags (control[0]) = 0 and Add Context Flags
    // (control[1]) = 0x3 (Slot Context A0 and Endpoint 0 Context A1). Because BSR=1 enumeration
    // already ran control transfers on EP0 while in the Default state, resynchronize EP0's TR
    // Dequeue Pointer/DCS to the current software transfer ring position while preserving
    // MAX_PACKET_SIZE.
    zx::result<CRCR> trb_phys =
        state->GetEndpoint().transfer_ring().PeekCommandRingControlRegister(cap_length_);
    if (trb_phys.is_error()) {
      return fpromise::make_error_promise(trb_phys.error_value());
    }
    uint32_t* control = static_cast<uint32_t*>(state->GetInputContext()->virt());
    control[0] = 0;
    control[1] = 0x3;
    auto* endpoint_context = reinterpret_cast<EndpointContext*>(
        reinterpret_cast<unsigned char*>(control) + (slot_size_bytes_ * 2));
    endpoint_context->Init(EndpointContext::Control, trb_phys.value(),
                           static_cast<uint16_t>(endpoint_context->MAX_PACKET_SIZE()));
    cmd.ptr = state->GetInputContext()->phys()[0];
  }
  cmd.set_BSR(0);
  cmd.set_SlotID(slot_id);
  auto slot_context = command_ring_.AllocateContext();
  if (!slot_context) {
    fdf::error("No memory.");
    return fpromise::make_result_promise(
               fpromise::result<TRB*, zx_status_t>(fpromise::error(ZX_ERR_NO_MEMORY)))
        .box();
  }
  slot_context->port_number = port_id;
  DataMemoryBarrier();
  return SubmitCommand(cmd, std::move(slot_context));
}

std::optional<usb_speed_t> UsbXhci::GetDeviceSpeed(uint8_t slot) {
  auto state = slot > 0 ? GetDeviceState(slot - 1) : nullptr;
  if (!state) {
    fdf::error("{} expects that slot is in use. state should not be nullptr", __func__);
    return std::nullopt;
  }
  {
    fbl::AutoLock _(&state->transaction_lock());
    if (state->IsDisconnecting()) {
      return std::nullopt;
    }
    if (state->GetHubLocked()) {
      return state->GetHubLocked()->speed;
    }
  }
  return static_cast<usb_speed_t>(
      PORTSC::Get(cap_length_, state->GetPort()).ReadFrom(&mmio_.value()).PortSpeed());
}

usb_speed_t UsbXhci::GetPortSpeed(uint8_t port_id) const {
  return static_cast<usb_speed_t>(
      PORTSC::Get(cap_length_, port_id).ReadFrom(&mmio_.value()).PortSpeed());
}

TRBPromise UsbXhci::AddressDeviceCommand(uint8_t slot_id, uint8_t port_id,
                                         std::optional<HubInfo> hub_info, bool bsr) {
  if (!IsValidSlotId(slot_id)) {
    return fpromise::make_error_promise(ZX_ERR_INVALID_ARGS);
  }
  fbl::RefPtr<DeviceState> state = GetDeviceState(SlotIdToDeviceId(slot_id));
  if (!state) {
    fdf::error("{} expects that slot is in use. state should not be nullptr", __func__);
    return fpromise::make_error_promise(ZX_ERR_IO_NOT_PRESENT);
  }
  return state->AddressDeviceCommand(this, slot_id, port_id, hub_info, dcbaa_, InterrupterMapping(),
                                     &command_ring_, &mmio_.value(), bsr);
}

void UsbXhci::SetDeviceInformation(uint8_t slot, uint8_t port, const std::optional<HubInfo>& hub) {
  auto state = fbl::MakeRefCounted<DeviceState>(slot - 1, this);
  {
    fbl::AutoLock _(&state->transaction_lock());
    state->SetDeviceInformation(slot, port, hub);
    if (hub) {
      auto hub_state = hub->hub_state;
      // Here, the hub_state is expected to be different from the device's state,
      // otherwise a double-acquire occurs.
      ZX_ASSERT(hub_state != state);
      fbl::AutoLock _(&hub_state->transaction_lock());
      if (hub_state->IsDisconnecting()) {
        return;
      }
      if (auto& parent_hub = hub_state->GetHubLocked(); parent_hub.has_value()) {
        parent_hub->port_to_device[port - 1] = static_cast<uint8_t>(slot - 1);
      }
    }
  }
  SetDeviceState(slot - 1, std::move(state));
}

TRBPromise UsbXhci::SetMaxPacketSizeCommand(uint8_t slot_id, uint8_t bMaxPacketSize0) {
  if (!IsValidSlotId(slot_id)) {
    return fpromise::make_error_promise(ZX_ERR_INVALID_ARGS);
  }
  fbl::RefPtr<DeviceState> state = GetDeviceState(SlotIdToDeviceId(slot_id));
  if (!state) {
    fdf::error("{} expects that slot is in use. state should not be nullptr", __func__);
    return fpromise::make_error_promise(ZX_ERR_IO_NOT_PRESENT);
  }
  usb_xhci::AddressDeviceStruct cmd;
  {
    fbl::AutoLock _(&state->transaction_lock());
    if (state->IsDisconnecting() || !state->GetInputContext()) {
      return fpromise::make_error_promise(ZX_ERR_IO_NOT_PRESENT);
    }
    // Per Intel xHCI 1.2 Specification Sections 4.6.7, 6.2.3.3, 6.2.5.1, and 6.4.3.6
    // (https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf),
    // Evaluate Context requires all Drop Context Flags (control[0]) = 0 and Add Context Flag A1
    // (control[1] = 0x2) set so the xHC re-evaluates only Endpoint 0's Max Packet Size without
    // picking up stale flags from prior InputContext commands.
    uint32_t* control = static_cast<uint32_t*>(state->GetInputContext()->virt());
    control[0] = 0;
    control[1] = 0x2;
    auto endpoint_context = reinterpret_cast<EndpointContext*>(
        reinterpret_cast<unsigned char*>(control) + (slot_size_bytes_ * 2));
    endpoint_context->set_MAX_PACKET_SIZE(bMaxPacketSize0);
    Control::Get().FromValue(0).set_Type(Control::EvaluateContextCommand).ToTrb(&cmd);
    cmd.set_SlotID(slot_id);

    cmd.ptr = state->GetInputContext()->phys()[0];
  }
  auto context = command_ring_.AllocateContext();
  return SubmitCommand(cmd, std::move(context));
}

zx_status_t UsbXhci::DeviceOnline(uint32_t slot, uint16_t port, usb_speed_t speed) {
  fdf::debug("Device online. slot - {} port - {}", slot, port);
  fbl::RefPtr<DeviceState> state = slot > 0 ? GetDeviceState(slot - 1) : nullptr;
  if (!state) {
    fdf::error("{} expects that slot is in use. state should not be nullptr", __func__);
    return ZX_ERR_IO_NOT_PRESENT;
  }
  uint32_t hub_id = 0;
  bool is_root_hub = false;
  bool is_usb_3 = false;
  {
    fbl::AutoLock transaction_lock(&state->transaction_lock());
    if (state->IsDisconnecting()) {
      return ZX_ERR_IO_NOT_PRESENT;
    }
    const std::optional<HubInfo>& hub = state->GetHubLocked();
    if (!hub.has_value() || !hub->hub_state) {
      is_root_hub = true;
      is_usb_3 = GetPortState(port).is_USB3;
      hub_id = static_cast<uint32_t>(is_usb_3 ? UsbHciGetMaxDeviceCount() - 1
                                              : UsbHciGetMaxDeviceCount() - 2);
    } else {
      hub_id = SlotIdToDeviceId(hub->hub_state->GetSlot());
    }
  }
  return PostCallback([this, slot, port, speed, hub_id, is_root_hub, is_usb_3,
                       state = std::move(state)] {
    {
      fbl::AutoLock _(&state->transaction_lock());
      if (state->IsDisconnecting()) {
        return;
      }
    }
    ddk::UsbBusInterfaceProtocolClient bus = SnapshotBusClient();
    if (IsHaltedOrStopping() || !bus.is_valid()) {
      return;
    }
    if (is_root_hub) {
      fdf::info("xhci: DeviceOnline slot={} port={} speed={} is_usb_3={} -> calling bus.AddDevice",
                slot, port, speed, is_usb_3);
    }
    if (bus.AddDevice(slot - 1, hub_id, speed) == ZX_OK) {
      bool already_removed = false;
      {
        fbl::AutoLock _(&state->transaction_lock());
        already_removed = !state->MarkBusDeviceAddedLocked();
      }
      // If UsbHciHubDeviceRemoved() ran reentrantly on ddk_interaction_dispatcher_ during
      // bus.AddDevice() (taking the inline fast path and marking kRemoved while skipping
      // bus.RemoveDevice()), remove the newly registered device now so usb-bus does not retain an
      // orphan. Non-reentrant disconnects remain in kNotAdded until their queued
      // RemoveSubtreeFromBusOnDdkDispatcher() runs next on ddk_interaction_dispatcher_.
      if (already_removed && running_) {
        ddk::UsbBusInterfaceProtocolClient current_bus = SnapshotBusClient();
        if (current_bus.is_valid()) {
          zx_status_t remove_status = current_bus.RemoveDevice(slot - 1);
          if (remove_status != ZX_OK) {
            fdf::warn("DeviceOnline reentrant cleanup bus.RemoveDevice({}) failed: {}", slot - 1,
                      zx_status_get_string(remove_status));
          }
        }
      }
    }
  })
             ? ZX_OK
             : ZX_ERR_IO_NOT_PRESENT;
}

fpromise::promise<void, zx_status_t> UsbXhci::DeviceOffline(uint32_t slot) {
  if (slot == 0 || slot > max_slots_) {
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_INVALID_ARGS);
  }
  auto state = GetDeviceState(slot - 1);
  if (!state) {
    fdf::error("{} expects that slot is in use. state should not be nullptr", __func__);
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO_NOT_PRESENT);
  }
  {
    fbl::AutoLock _(&state->transaction_lock());
    if (state->IsDisconnecting()) {
      return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO_NOT_PRESENT);
    }
  }
  // Disconnect the device and every device downstream of it if it is a hub. Disable the slots
  // before completing pending requests. Completing a request unpins its buffers, and the
  // controller may still be accessing them until the slot is disabled.
  auto targets = CollectDisconnectSubtree(state);
  std::vector<fpromise::promise<void, zx_status_t>> disable_promises;
  disable_promises.reserve(targets.size());
  for (auto& t : targets) {
    disable_promises.push_back(DisableSlotCommand(*t, t));
  }
  return fpromise::join_promise_vector(std::move(disable_promises))
      .then([this, targets = std::move(targets)](
                fpromise::result<std::vector<fpromise::result<void, zx_status_t>>>& results) mutable
                -> fpromise::promise<void, zx_status_t> {
        std::vector<fbl::RefPtr<DeviceState>> removable_downstream;
        zx_status_t root_error = ZX_OK;
        const bool root_drained = DrainDisabledSubtree(targets, results, "DeviceOffline",
                                                       &removable_downstream, &root_error);
        if (!root_drained && removable_downstream.empty()) {
          return !running_ ? fpromise::make_result_promise<void, zx_status_t>(fpromise::ok())
                           : fpromise::make_error_promise<zx_status_t>(root_error);
        }
        fbl::RefPtr<DeviceState> root = root_drained ? targets[0] : nullptr;
        fpromise::bridge<void, zx_status_t> bridge;
        // Use bridge.consumer.promise_or(ZX_ERR_IO_NOT_PRESENT) so that if PostCallback() rejects
        // the task during teardown or Stop() shuts down ddk_interaction_dispatcher_ before the
        // task runs, the promise resolves with ZX_ERR_IO_NOT_PRESENT instead of abandoning
        // downstream continuations.
        PostCallback([this, root = std::move(root),
                      removable_downstream = std::move(removable_downstream), root_error,
                      completer = std::move(bridge.completer)] mutable {
          if (root) {
            if (root->slot_disabled_.Wait(zx::duration::infinite()) != ZX_OK) {
              Shutdown(ZX_ERR_BAD_STATE);
            }
          }
          for (auto& t : removable_downstream) {
            if (t->slot_disabled_.Wait(zx::duration::infinite()) != ZX_OK) {
              Shutdown(ZX_ERR_BAD_STATE);
            }
          }
          const zx_status_t status =
              RemoveSubtreeFromBusOnDdkDispatcher(root, removable_downstream, root_error);
          if (status != ZX_OK) {
            completer.complete_error(status);
            return;
          }
          completer.complete_ok();
        });
        return bridge.consumer.promise_or(fpromise::error(ZX_ERR_IO_NOT_PRESENT));
      })
      .box();
}

void UsbXhci::CreateDeviceInspectNode(uint32_t slot, uint16_t vendor_id, uint16_t product_id) {
  auto state = slot > 0 ? GetDeviceState(slot - 1) : nullptr;
  if (!state) {
    fdf::error("{} expects that slot is in use. state should not be nullptr", __func__);
    return;
  }

  char name[64];
  snprintf(name, sizeof(name), "Device %hu", slot);
  auto node = inspect_root_node().CreateChild(name);
  state->CreateInspectNode(std::move(node), vendor_id, product_id);
}

void UsbXhci::ResetPort(uint16_t port) {
  PORTSC sc = PORTSC::Get(cap_length_, port).ReadFrom(&mmio_.value());
  PORTSC::Get(cap_length_, port)
      .FromValue(0)
      .set_CCS(sc.CCS())
      .set_PortSpeed(sc.PortSpeed())
      .set_PIC(sc.PIC())
      .set_PLS(sc.PLS())
      .set_PP(sc.PP())
      .set_PR(1)
      .WriteTo(&mmio_.value());
}

fpromise::promise<void, zx_status_t> UsbXhci::UsbHciHubDeviceAddedAsync(uint32_t device_id,
                                                                        uint32_t port,
                                                                        usb_speed_t speed) {
  auto state = GetDeviceState(device_id);
  if (!state) {
    fdf::error("{} expects that slot is in use. state should not be nullptr", __func__);
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO_NOT_PRESENT);
  }
  // Acquire a slot
  HubInfo hub;
  {
    fbl::AutoLock _(&state->transaction_lock());
    hub.hub_state = state;
    hub.speed = speed;
    hub.parent_port_number = static_cast<uint8_t>(port);
    if (state->GetHubLocked()) {
      hub.multi_tt = state->GetHubLocked()->multi_tt;
      hub.route_string = (state->GetHubLocked()->route_string) |
                         ((port) << (state->GetHubLocked()->hub_depth * 4));
      hub.hub_depth = static_cast<uint8_t>(state->GetHubLocked()->hub_depth);
      hub.hub_speed = static_cast<uint8_t>(state->GetHubLocked()->speed);
      hub.rh_port = state->GetHubLocked()->rh_port;
      hub.tt_info = state->GetHubLocked()->tt_info;
    }
  }
  fdf::info(
      "Hub found: Speed: {}, Port: {}, Parent port:{} "
      "Route String:{}, Multi_tt:{}, Device ID:{}",
      hub.hub_speed, port, hub.parent_port_number, hub.route_string, hub.multi_tt, device_id);

  return EnumerateDevice(this, static_cast<uint8_t>(port), std::make_optional(hub));
}

fpromise::promise<void, zx_status_t> UsbXhci::ConfigureHubAsync(uint32_t device_id,
                                                                usb_speed_t speed,
                                                                const usb_hub_descriptor_t* desc,
                                                                bool multi_tt) {
  if (!desc || !IsValidDeviceId(device_id)) {
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_INVALID_ARGS);
  }
  auto state = GetDeviceState(device_id);
  if (!state) {
    fdf::error("{} expects that slot is in use. state should not be nullptr", __func__);
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO_NOT_PRESENT);
  }
  HubInfo hub;
  std::optional<HubInfo> prev_hub;
  uint32_t prev_context_entries = 0;
  uint32_t prev_speed = 0;
  uint32_t prev_multi_tt = 0;
  uint32_t prev_hub_bit = 0;
  uint32_t prev_port_count = 0;
  uint32_t prev_ttt = 0;
  uint32_t* control = nullptr;
  SlotContext* slot_context = nullptr;
  struct AddressDeviceStruct cmd;
  std::unique_ptr<TRBContext> context;
  {
    fbl::AutoLock _(&state->transaction_lock());
    if (state->IsDisconnecting()) {
      return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO_NOT_PRESENT);
    }
    prev_hub = state->GetHubLocked();
    hub.speed = speed;
    hub.hub_speed = speed;
    hub.multi_tt = (speed == fdescriptor::UsbSpeed::kHigh) && multi_tt;
    hub.hub_configured = true;
    hub.rh_port = state->GetPort();
    if (state->GetHubLocked()) {
      hub.hub_state = state->GetHubLocked()->hub_state;
      hub.parent_port_number = state->GetHubLocked()->parent_port_number;
      hub.route_string = state->GetHubLocked()->route_string;
      hub.hub_depth = state->GetHubLocked()->hub_configured
                          ? state->GetHubLocked()->hub_depth
                          : static_cast<uint8_t>(state->GetHubLocked()->hub_depth + 1);
      hub.rh_port = state->GetHubLocked()->rh_port;
      hub.tt_info = state->GetHubLocked()->tt_info;
      if (state->GetHubLocked()->hub_configured) {
        std::copy(std::begin(state->GetHubLocked()->port_to_device),
                  std::end(state->GetHubLocked()->port_to_device), std::begin(hub.port_to_device));
      }
    }
    state->GetHubLocked() = hub;
    uint8_t slot = state->GetSlot();
    // Per Intel xHCI 1.2 Specification Section 6.2.2 (Tables 6-4, 6-5, 6-6) and Section 6.2.2.2
    // (Configure Endpoint Command Usage,
    // https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf),
    // Configure Endpoint with A0 = 1 requires Context Entries >= 1 (preserving any higher active
    // endpoint DCI already enabled) and MTT = 1 only for a High-Speed hub with Multi-TT enabled
    // (when Hub = 1 on a non-High-Speed hub, MTT must be 0; ancestor TT state for downstream
    // Low-/Full-Speed children is preserved in hub.tt_info).
    control = static_cast<uint32_t*>(state->GetInputContext()->virt());
    control[0] = 0;
    control[1] = 1;
    slot_context = reinterpret_cast<SlotContext*>(reinterpret_cast<unsigned char*>(control) +
                                                  slot_size_bytes_);
    prev_context_entries = slot_context->CONTEXT_ENTRIES();
    prev_speed = slot_context->SPEED();
    prev_multi_tt = slot_context->MULTI_TT();
    prev_hub_bit = slot_context->HUB();
    prev_port_count = slot_context->PORT_COUNT();
    prev_ttt = slot_context->TTT();
    slot_context->set_CONTEXT_ENTRIES(std::max<uint32_t>(slot_context->CONTEXT_ENTRIES(), 1))
        .set_SPEED(speed)
        .set_MULTI_TT(hub.multi_tt)
        .set_HUB(1)
        .set_PORT_COUNT(desc->b_nbr_ports)
        .set_TTT((speed == fdescriptor::UsbSpeed::kHigh)
                     ? ((le16toh(desc->w_hub_characteristics) >> 5) & 3)
                     : 0);
    // Use ConfigureEndpointCommand according to sections 6.2.2.2 and 6.2.2.3.
    Control::Get().FromValue(0).set_Type(Control::ConfigureEndpointCommand).ToTrb(&cmd);
    cmd.set_SlotID(slot).set_BSR(0);
    cmd.ptr = state->GetInputContext()->phys()[0];
    DataMemoryBarrier();
    context = command_ring_.AllocateContext();
  }
  return SubmitCommand(cmd, std::move(context))
      .then([=, this](fpromise::result<TRB*, zx_status_t>& result)
                -> fpromise::promise<void, zx_status_t> {
        auto rollback_hub = fit::defer([=]() {
          fbl::AutoLock _(&state->transaction_lock());
          if (!state->IsDisconnecting()) {
            state->GetHubLocked() = prev_hub;
            if (state->GetInputContext()) {
              control[0] = 0;
              control[1] = 0;
              slot_context->set_CONTEXT_ENTRIES(prev_context_entries)
                  .set_SPEED(prev_speed)
                  .set_MULTI_TT(prev_multi_tt)
                  .set_HUB(prev_hub_bit)
                  .set_PORT_COUNT(prev_port_count)
                  .set_TTT(prev_ttt);
            }
          }
        });
        if (result.is_error()) {
          return fpromise::make_error_promise<zx_status_t>(result.error());
        }
        auto completion = reinterpret_cast<CommandCompletionEvent*>(result.value());
        if (completion->CompletionCode() != CommandCompletionEvent::Success) {
          fdf::error("Failed to configure endpoint: CompletionCode() == {}",
                     completion->CompletionCode());
          return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO);
        }
        rollback_hub.cancel();
        if (speed != fdescriptor::UsbSpeed::kSuper) {
          return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
        }
        std::optional<usb::Request<void>> request_wrapper;
        zx_status_t status =
            usb::Request<void>::Alloc(&request_wrapper, 0, 0, UsbHciGetRequestSize());
        if (status != ZX_OK) {
          return fpromise::make_error_promise<zx_status_t>(status);
        }
        usb_request_t* request = request_wrapper->request();
        request->direct = true;
        request->header.device_id = device_id;
        request->header.ep_address = 0;
        request->setup.bm_request_type = kClassDeviceOut;
        {
          fbl::AutoLock _(&state->transaction_lock());
          if (state->IsDisconnecting()) {
            return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO_NOT_PRESENT);
          }
          request->setup.w_value = state->GetHubLocked()->hub_depth;
        }
        request->setup.w_index = 0;
        request->setup.b_request = USB_HUB_SET_DEPTH;
        request->setup.w_length = 0;
        return UsbHciRequestQueue(std::move(*request_wrapper))
            .then(
                [](fpromise::result<OwnedRequest>& result) -> fpromise::result<void, zx_status_t> {
                  if (result.value().request()->response.status != ZX_OK) {
                    return fpromise::error(result.value().request()->response.status);
                  }
                  return fpromise::ok();
                });
      })
      .box();
}

void UsbXhci::DisconnectAllDevices() const {
  const size_t slot_count = DeviceStateCount();
  for (size_t i = 0; i < slot_count; i++) {
    if (auto state = GetDeviceState(static_cast<uint32_t>(i))) {
      fbl::AutoLock _(&state->transaction_lock());
      state->Disconnect();
    }
  }
}

void UsbXhci::ResetAllDevices() const {
  const size_t slot_count = DeviceStateCount();
  for (size_t i = 0; i < slot_count; i++) {
    if (auto state = GetDeviceState(static_cast<uint32_t>(i))) {
      state->ResetHci();
    }
  }
}

void UsbXhci::ShutdownDdkInteractionDispatcher() {
  {
    fbl::AutoLock _(&ddk_interaction_lock_);
    ddk_interaction_shutting_down_ = true;
    if (!ddk_interaction_dispatcher_.get()) {
      return;
    }
    ddk_interaction_dispatcher_.ShutdownAsync();
  }
  ddk_interaction_shutdown_.Wait();
  fdf::SynchronizedDispatcher dispatcher_to_destroy;
  {
    fbl::AutoLock _(&ddk_interaction_lock_);
    dispatcher_to_destroy = std::move(ddk_interaction_dispatcher_);
  }
}

void UsbXhci::HaltController(std::string_view phase) {
  if (!mmio_.has_value()) {
    return;
  }
  // Clear USBCMD.ENABLE (RS), USBCMD.INTE, and USBCMD.HSEE and mask IMAN(i).IE across all
  // interrupters before tearing down interrupters so no further hardware interrupts fire.
  USBCMD::Get(cap_length_)
      .ReadFrom(&mmio_.value())
      .set_ENABLE(0)
      .set_INTE(0)
      .set_HSEE(0)
      .WriteTo(&mmio_.value());
  for (size_t i = 0; i < interrupters_.size(); i++) {
    IMAN::Get(runtime_offset_, static_cast<uint16_t>(i))
        .ReadFrom(&mmio_.value())
        .set_IE(0)
        .WriteTo(&mmio_.value());
  }
  constexpr zx::duration kStopTimeout = zx::msec(50);
  constexpr zx::duration kPollInterval = zx::usec(250);
  const zx::time deadline = zx::deadline_after(kStopTimeout);
  while (!USBSTS::Get(cap_length_).ReadFrom(&mmio_.value()).HCHalted()) {
    if (zx::clock::get_monotonic() >= deadline) {
      fdf::error(
          "Timed out waiting for xHCI controller to halt during {}; keeping active DMA buffers "
          "pinned",
          phase);
      break;
    }
    zx::nanosleep(zx::deadline_after(kPollInterval));
  }
}

void UsbXhci::Stop(fdf::StopCompleter completer) {
  fdf::info("xhci: Stop() entered, signaling bringup_ and stopping interrupters");
  running_ = false;
  sync_completion_signal(&bringup_);
  const bool has_hardware = HasDeviceState() && mmio_.has_value();
  if (has_hardware) {
    HaltController("Stop");
  }
  for (auto& it : interrupters_) {
    it.Stop();
  }
  for (auto& it : interrupters_) {
    it.ring().ClearPendingPortStatusChanges();
  }
  // Mark every slot disconnecting under its transaction_lock() before draining so no concurrent
  // request queue call can enqueue new TRBs once teardown draining starts.
  DisconnectAllDevices();
  DrainUntilQuiescent();
  ShutdownDdkInteractionDispatcher();
  DrainUntilQuiescent();
  if (has_hardware && !IsControllerHalted()) {
    fdf::error(
        "xhci: Stop() timed out waiting for controller halt; active DMA buffers remain pinned");
    completer(zx::error(ZX_ERR_TIMED_OUT));
    return;
  }
  if (IsControllerHalted()) {
    ResetAllDevices();
  }
  fdf::info("xhci: Stop() completed cleanly");
  completer(zx::ok());
}

// Per the Intel xHCI 1.2 Specification, Sections 5.4.1 (Table 5-20, USBCMD.R/S) and 5.4.2 (Table
// 5-21, USBSTS.HCH,
// https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf),
// clearing R/S stops the xHC within 16 ms after pending pipelined USB transactions finish,
// asserting HCH = 1. Only complete/unpin endpoint Transfer Ring requests once HCH = 1 is confirmed;
// if a halt timed out, leave active transfer rings and client DMA buffers pinned (quarantined).
bool UsbXhci::DrainPendingOnce() {
  auto trbs = command_ring_.TakePendingTRBs();
  bool pending = !trbs.is_empty();
  for (auto& trb : trbs) {
    if (trb.completer.has_value()) {
      trb.completer.value().complete_error(ZX_ERR_IO_NOT_PRESENT);
    }
  }
  if (!HasDeviceState() || !IsHaltedOrStopping() || !IsControllerHalted()) {
    return pending;
  }
  const size_t slot_count = DeviceStateCount();
  for (size_t i = 0; i < slot_count; i++) {
    auto state = GetDeviceState(static_cast<uint32_t>(i));
    if (!state) {
      continue;
    }
    fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> ep0_trbs;
    Endpoint* ep0 = nullptr;
    {
      fbl::AutoLock _(&state->transaction_lock());
      ep0 = &state->GetEndpoint();
      ep0_trbs = ep0->transfer_ring().TakePendingTRBs();
    }
    pending |= !ep0_trbs.is_empty();
    for (auto& trb : ep0_trbs) {
      if (trb.request.has_value()) {
        ep0->RequestComplete(ZX_ERR_IO_NOT_PRESENT, 0, std::move(*trb.request));
      }
    }
    for (uint8_t c = 0; c < kMaxEndpoints; c++) {
      fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> ep_trbs;
      Endpoint* ep = nullptr;
      {
        fbl::AutoLock _(&state->transaction_lock());
        ep = state->TryGetEndpoint(c);
        if (!ep) {
          continue;
        }
        ep_trbs = ep->transfer_ring().TakePendingTRBs();
      }
      pending |= !ep_trbs.is_empty();
      for (auto& trb : ep_trbs) {
        if (trb.request.has_value()) {
          ep->RequestComplete(ZX_ERR_IO_NOT_PRESENT, 0, std::move(*trb.request));
        }
      }
    }
  }
  return pending;
}

void UsbXhci::DrainUntilQuiescent() {
  // Flush any outstanding async I/O
  // TODO(https://fxbug.dev/42120782): Migrate to joins
  auto pump_interrupters = [this] {
    for (size_t pass = 0; pass < interrupters_.size(); ++pass) {
      for (auto& it : interrupters_) {
        it.ring().RunUntilIdle();
      }
    }
  };
  pump_interrupters();
  while (DrainPendingOnce()) {
    pump_interrupters();
  }
}

void UsbXhci::AbortPendingCommands() { (void)DrainPendingOnce(); }

UsbXhci::~UsbXhci() {
  running_ = false;
  sync_completion_signal(&bringup_);
  if (!mmio_.has_value() || !HasDeviceState()) {
    ShutdownDdkInteractionDispatcher();
    return;
  }

  if (USBCMD::Get(cap_length_).ReadFrom(&mmio_.value()).ENABLE()) {
    HaltController("destruction");
  }
  // Disable all interrupters
  for (auto& it : interrupters_) {
    it.Stop();
  }
  for (auto& it : interrupters_) {
    it.ring().ClearPendingPortStatusChanges();
  }
  DisconnectAllDevices();
  DrainUntilQuiescent();
  ShutdownDdkInteractionDispatcher();
  DrainUntilQuiescent();

  if (!IsControllerHalted()) {
    // Deliberately terminate the driver host if hardware failed to halt so Zircon closes the BTI
    // and quarantines all pinned DMA pages (DCBAA, scratchpads, command/event rings, and active
    // device transfer rings) instead of unpinning them in destructors while hardware may still DMA.
    fdf::error(
        "xHCI controller did not halt before destruction; self-terminating to quarantine pinned "
        "DMA pages");
    ZX_ASSERT_MSG(false, "xHCI controller failed to halt with DMA buffers pinned");
  }

  ResetAllDevices();
  (void)TakeAllDeviceStates();
  interrupters_.reset();
}

// USB HCI protocol implementation.
void UsbXhci::UsbHciRequestQueue(usb_request_t* usb_request,
                                 const usb_request_complete_callback_t* complete_cb) {
  Request request(usb_request, *complete_cb, sizeof(usb_request_t));

  request.TraceFlow();

  if (!IsValidDeviceId(request.request()->header.device_id)) {
    request.Complete(ZX_ERR_INVALID_ARGS, 0);
    return;
  }
  uint8_t ep_addr = request.request()->header.ep_address;
  if (!IsValidEndpointAddress(ep_addr)) {
    request.Complete(ZX_ERR_INVALID_ARGS, 0);
    return;
  }
  if (!running_) {
    request.Complete(ZX_ERR_IO_NOT_PRESENT, 0);
    return;
  }
  auto state = GetDeviceState(request.request()->header.device_id);
  if (!state) {
    fdf::error("{} expects that slot is in use. state should not be nullptr", __func__);
    request.Complete(ZX_ERR_IO_NOT_PRESENT, 0);
    return;
  }
  Endpoint* ep;
  uint8_t slot;
  uint8_t ep_index = XhciEndpointIndex(ep_addr);
  {
    fbl::AutoLock lock(&state->transaction_lock());
    slot = state->GetSlot();
    if (!running_ || !slot || state->IsDisconnecting()) {
      lock.release();
      request.Complete(ZX_ERR_IO_NOT_PRESENT, 0);
      return;
    }
    if (unlikely(ep_addr == 0)) {
      ep = &state->GetEndpoint();
    } else {
      ep = &state->GetEndpoint(ep_index - 1);
    }
  }
  ep->QueueRequest(std::move(request));
  if (!running_) {
    return;
  }
  if (unlikely(ep_addr == 0)) {
    RingDoorbell(slot, 1);
  } else {
    RingDoorbell(slot, 2 + (ep_index - 1));
  }
}

void UsbXhci::UsbHciSetBusInterface(const usb_bus_interface_protocol_t* bus_intf) {
  // Value is semi arbitrary. The value only needs to be large enough that it is considered unusual
  // that the system is halted for any longer
  static const zx::duration kAbnormalHaltedDuration = zx::sec(12);

  struct FenceState {
    std::atomic_bool ran = false;
    libsync::Completion done;
  };
  std::shared_ptr<FenceState> fence;
  bool clearing = false;
  bool wait_for_shutdown = false;
  {
    fbl::AutoLock _(&ddk_interaction_lock_);
    if (bus_intf == nullptr || bus_intf->ops == nullptr || bus_.is_valid()) {
      // Clear bus_ when unbinding (null bus_intf, or re-invoked while bus_ is already valid).
      clearing = true;
    } else {
      bus_ = bus_intf;
    }

    if (clearing) {
      // Clear bus_ and enqueue a barrier task under the same lock acquisition so no callback
      // queued after this point can snapshot the previous bus_ client and any already-running or
      // queued callback finishes before UsbHciSetBusInterface() returns.
      bus_.clear();
      if (ddk_interaction_started_ && !IsOnDdkInteractionDispatcherLocked()) {
        if (ddk_interaction_shutting_down_ || !ddk_interaction_dispatcher_.get()) {
          wait_for_shutdown = true;
        } else {
          fence = std::make_shared<FenceState>();
          const zx_status_t status =
              async::PostTask(ddk_interaction_dispatcher_.async_dispatcher(),
                              [fence, signal = fit::defer([fence] { fence->done.Signal(); })] {
                                fence->ran.store(true, std::memory_order_release);
                              });
          if (status != ZX_OK) {
            fence.reset();
            wait_for_shutdown = true;
          }
        }
      }
    }
  }
  if (clearing) {
    if (bus_intf == nullptr || bus_intf->ops == nullptr) {
      fdf::info("xhci: UsbHciSetBusInterface clearing bus_ interface on unbind");
    } else {
      fdf::info("xhci: UsbHciSetBusInterface clearing existing bus_ interface");
    }
    if (fence) {
      fence->done.Wait();
      if (!fence->ran.load(std::memory_order_acquire)) {
        wait_for_shutdown = true;
      }
    }
    if (wait_for_shutdown) {
      ddk_interaction_shutdown_.Wait();
    }
    return;
  }

  USBCMD::Get(cap_length_)
      .ReadFrom(&mmio_.value())
      .set_ENABLE(1)
      .set_INTE(1)
      .set_HSEE(1)
      .set_EWE(0)
      .WriteTo(&mmio_.value());
  const zx::time start_time = zx::clock::get_monotonic();
  bool logged = false;
  while (USBSTS::Get(cap_length_).ReadFrom(&mmio_.value()).HCHalted()) {
    zx::nanosleep(zx::deadline_after(zx::msec(1)));
    const zx::duration delta_time = zx::clock::get_monotonic() - start_time;
    if (delta_time > kAbnormalHaltedDuration && !logged) {
      fdf::info("Still halted after {} seconds", delta_time.to_secs());
      logged = true;
    }
  }

  sync_completion_signal(&bringup_);
  fdf::info("xhci: UsbHciSetBusInterface completed, signaled bringup_");
}
size_t UsbXhci::UsbHciGetMaxDeviceCount() {
  // Last two slots represent the virtual hubs (USB 2.0 and 3.0 respectively)
  return params_.MaxSlots() + 2;
}

zx_status_t UsbXhci::UsbHciEnableEndpoint(uint32_t device_id,
                                          const usb_endpoint_descriptor_t* ep_desc,
                                          const usb_ss_ep_comp_descriptor_t* ss_com_desc,
                                          bool enable) {
  if (!ep_desc) {
    return ZX_ERR_INVALID_ARGS;
  }

  if (!running_) {
    return ZX_ERR_IO_NOT_PRESENT;
  }
  if (!IsValidDeviceId(device_id)) {
    // TODO: Root hub endpoint support
    return ZX_ERR_OUT_OF_RANGE;
  }
  if (!IsValidEndpointAddress(ep_desc->b_endpoint_address) || ep_desc->b_endpoint_address == 0) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (!enable) {
    return RunSynchronously(kPrimaryInterrupter,
                            UsbHciDisableEndpoint(device_id, ep_desc, ss_com_desc));
  }
  return RunSynchronously(kPrimaryInterrupter,
                          UsbHciEnableEndpoint(device_id, ep_desc, ss_com_desc));
}

fpromise::promise<void, zx_status_t> UsbXhci::UsbHciEnableEndpoint(
    uint32_t device_id, const usb_endpoint_descriptor_t* ep_desc,
    const usb_ss_ep_comp_descriptor_t* ss_com_desc) {
  if (!ep_desc || !IsValidDeviceId(device_id)) {
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_INVALID_ARGS);
  }
  if (!IsValidEndpointAddress(ep_desc->b_endpoint_address) || ep_desc->b_endpoint_address == 0) {
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_INVALID_ARGS);
  }
  auto context = command_ring_.AllocateContext();
  auto state = GetDeviceState(device_id);
  if (!state) {
    fdf::error("{} expects that slot is in use. state should not be nullptr", __func__);
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO_NOT_PRESENT);
  }
  uint32_t* control;
  SlotContext* slot_context;
  EndpointContext* endpoint_context;
  TRB trb;
  uint32_t context_entries;
  uint8_t index;
  {
    fbl::AutoLock _(&state->transaction_lock());
    if (state->IsDisconnecting()) {
      return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO_NOT_PRESENT);
    }
    control = reinterpret_cast<uint32_t*>(state->GetInputContext()->virt());

    // Initialize input slot context data structure (6.2.2) with 1 context entry
    // Set root hub port number to port number and context entries to 1
    slot_context = reinterpret_cast<SlotContext*>(reinterpret_cast<unsigned char*>(control) +
                                                  slot_size_bytes_);
    context_entries = slot_context->CONTEXT_ENTRIES();
    index = XhciEndpointIndex(ep_desc->b_endpoint_address);
    if (index >= context_entries) {
      slot_context->set_CONTEXT_ENTRIES(index + 1);
    }
    // Allocate the transfer ring (see section 4.9)
    control[0] = 0;
    control[1] = 1 | (1 << (index + 1));
    zx_status_t status = state->InitEndpoint(
        ep_desc->b_endpoint_address, &this->interrupter(state->GetInterrupterTarget()).ring(),
        &mmio_.value());
    if (status != ZX_OK) {
      control[1] = 0;
      slot_context->set_CONTEXT_ENTRIES(context_entries);
      return fpromise::make_error_promise<zx_status_t>(status);
    }
    CRCR trb_phys = state->GetEndpoint(index - 1).transfer_ring().phys(cap_length_);
    // Initialize endpoint context 0
    // Set CERR to 3, TR dequeue pointer, max packet size, EP type = control, DCS = 1
    endpoint_context = reinterpret_cast<EndpointContext*>(
        reinterpret_cast<unsigned char*>(control) + (slot_size_bytes_ * (2 + index)));

    // See section 4.3.6
    fdescriptor::EndpointType ep_type = usb_ep_type(ep_desc);
    state->GetEndpoint(index - 1).set_ep_type(ep_type);
    if (usb_ep_is_isoch(ep_desc)) {
      state->GetEndpoint(index - 1).transfer_ring().SetIsochronous();
    }
    uint32_t ep_index = fidl::ToUnderlying(ep_type);
    if (usb_ep_is_in(ep_desc)) {
      ep_index += 4;
    }
    const uint32_t max_packet_size = le16toh(ep_desc->w_max_packet_size) & 0x07FF;
    endpoint_context->Init(static_cast<EndpointContext::EndpointType>(ep_index), trb_phys,
                           static_cast<uint16_t>(max_packet_size));
    int interval = ComputeInterval(ep_desc, slot_context->SPEED());
    if (interval == -1) {
      interval = 1;
    }
    endpoint_context->set_Interval(interval);
    // Section 6.2.3.4
    uint32_t max_burst = 0;
    if (ss_com_desc) {
      max_burst = ss_com_desc->b_max_burst;
    } else if ((slot_context->SPEED() == fdescriptor::UsbSpeed::kHigh) &&
               (usb_ep_is_isoch(ep_desc) || usb_ep_is_int(ep_desc))) {
      max_burst = (le16toh(ep_desc->w_max_packet_size) >> 11) & 3;
    }
    endpoint_context->set_MaxBurstSize(max_burst);
    if (usb_ep_is_isoch(ep_desc) || usb_ep_is_int(ep_desc)) {
      const uint32_t max_esit_payload =
          (ss_com_desc && le16toh(ss_com_desc->w_bytes_per_interval) > 0)
              ? le16toh(ss_com_desc->w_bytes_per_interval)
              : (max_packet_size * (max_burst + 1));
      endpoint_context->set_MAX_ESIT_PAYLOAD_LOW(max_esit_payload & 0xFFFF);
    }
    trb.ptr = state->GetInputContext()->phys()[0];
    Control::Get()
        .FromValue((device_id + 1) << 24)
        .set_Type(Control::ConfigureEndpointCommand)
        .ToTrb(&trb);
  }
  // TODO(https://fxbug.dev/42109415): Implement async support
  DataMemoryBarrier();
  return SubmitCommand(trb, std::move(context))
      .then(
          [=](fpromise::result<TRB*, zx_status_t>& result) -> fpromise::result<void, zx_status_t> {
            auto free_buffers = fit::defer([=]() {
              fbl::AutoLock _(&state->transaction_lock());
              if (!state->IsDisconnecting()) {
                control[0] = 0;
                control[1] = 0;
                endpoint_context->Deinit();
                slot_context->set_CONTEXT_ENTRIES(context_entries);
              }
              state->GetEndpoint(index - 1).transfer_ring().Deinit();
            });
            if (result.is_error()) {
              return fpromise::error(result.error());
            }
            auto completion = static_cast<CommandCompletionEvent*>(result.value());
            if (completion->CompletionCode() == CommandCompletionEvent::BandwidthError) {
              // TODO(https://fxbug.dev/42068887): We could handle this by implementing bandwidth
              // negotiation (see Section 4.16.1). For now just return an error to the client.
              return fpromise::error(ZX_ERR_NO_RESOURCES);
            }
            if (completion->CompletionCode() != CommandCompletionEvent::Success) {
              fdf::error("Failed to configure endpoint: CompletionCode() == {}",
                         completion->CompletionCode());
              return fpromise::error(ZX_ERR_IO);
            }
            free_buffers.cancel();
            return fpromise::ok();
          })
      .box();
}

fpromise::promise<void, zx_status_t> UsbXhci::UsbHciDisableEndpoint(
    uint32_t device_id, const usb_endpoint_descriptor_t* ep_desc,
    const usb_ss_ep_comp_descriptor_t* ss_com_desc) {
  if (!ep_desc) {
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_INVALID_ARGS);
  }
  return UsbHciDisableEndpoint(device_id, ep_desc->b_endpoint_address);
}

fpromise::promise<void, zx_status_t> UsbXhci::UsbHciDisableEndpoint(uint32_t device_id,
                                                                    uint8_t ep_addr) {
  if (!IsValidDeviceId(device_id) || !IsValidEndpointAddress(ep_addr) || ep_addr == 0) {
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_INVALID_ARGS);
  }
  auto context = command_ring_.AllocateContext();
  auto state = GetDeviceState(device_id);
  if (!state) {
    fdf::error("{} expects that slot is in use. state should not be nullptr", __func__);
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO_NOT_PRESENT);
  }
  uint8_t index = XhciEndpointIndex(ep_addr);
  SlotContext* slot_context;
  uint32_t prev_context_entries;
  TRB trb;
  uint32_t* control;
  {
    fbl::AutoLock _(&state->transaction_lock());
    if (state->IsDisconnecting()) {
      return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO_NOT_PRESENT);
    }
    control = reinterpret_cast<uint32_t*>(state->GetInputContext()->virt());
    // Set the Drop Context flag for the disabled endpoint and A0 = 1 for the Slot Context,
    // and update CONTEXT_ENTRIES to the highest remaining active endpoint context index (or 1
    // for EP0 if no non-EP0 endpoints remain active), per xHCI 1.2 section 6.2.2.2.
    control[0] = (1 << (index + 1));
    control[1] = 1;
    slot_context = reinterpret_cast<SlotContext*>(reinterpret_cast<unsigned char*>(control) +
                                                  slot_size_bytes_);
    prev_context_entries = slot_context->CONTEXT_ENTRIES();
    uint32_t max_context_entries = 1;
    for (uint8_t i = kMaxEndpoints - 2; i >= 1; --i) {
      if (i == index) {
        continue;
      }
      auto* ep_ctx = reinterpret_cast<EndpointContext*>(reinterpret_cast<unsigned char*>(control) +
                                                        (slot_size_bytes_ * (2 + i)));
      if (ep_ctx->EP_TYPE() != EndpointContext::Invalid) {
        max_context_entries = i + 1;
        break;
      }
    }
    slot_context->set_CONTEXT_ENTRIES(max_context_entries);
    trb.ptr = state->GetInputContext()->phys()[0];
    Control::Get()
        .FromValue((device_id + 1) << 24)
        .set_Type(Control::ConfigureEndpointCommand)
        .ToTrb(&trb);
  }
  // TODO(https://fxbug.dev/42109415): Implement async support
  DataMemoryBarrier();
  return SubmitCommand(trb, std::move(context))
      .then([=, this](fpromise::result<TRB*, zx_status_t>& result)
                -> fpromise::result<void, zx_status_t> {
        auto restore_context_entries = fit::defer([=]() {
          fbl::AutoLock _(&state->transaction_lock());
          if (!state->IsDisconnecting()) {
            control[0] = 0;
            control[1] = 0;
            slot_context->set_CONTEXT_ENTRIES(prev_context_entries);
          }
        });
        if (result.is_error()) {
          return fpromise::error(ZX_ERR_BAD_STATE);
        }
        auto completion = reinterpret_cast<CommandCompletionEvent*>(result.value());
        if (completion->CompletionCode() != CommandCompletionEvent::Success) {
          fdf::error("Failed to configure endpoint: CompletionCode() == {}",
                     completion->CompletionCode());
          return fpromise::error(ZX_ERR_BAD_STATE);
        }
        fbl::AutoLock _(&state->transaction_lock());
        if (state->IsDisconnecting()) {
          restore_context_entries.cancel();
          return fpromise::error(ZX_ERR_IO_NOT_PRESENT);
        }
        auto endpoint_context = reinterpret_cast<EndpointContext*>(
            reinterpret_cast<unsigned char*>(control) + (slot_size_bytes_ * (2 + index)));
        endpoint_context->Deinit();
        restore_context_entries.cancel();
        zx_status_t status = state->GetEndpoint(index - 1).transfer_ring().Deinit();
        // If we can't deinit the ring something is seriously wrong.
        if (status != ZX_OK) {
          return fpromise::error(ZX_ERR_BAD_STATE);
        }
        return fpromise::ok();
      })
      .box();
}

uint64_t UsbXhci::UsbHciGetCurrentFrame() {
  if (!running_) {
    return 0;
  }

  // Locking the logic below is important; without the lock, race conditions
  // were observed which led to "phantom" wraps being incorrectly counted.
  static fbl::Mutex lock;
  fbl::AutoLock _(&lock);

  uint32_t mfindex = MFINDEX::Get(runtime_offset_).ReadFrom(&mmio_.value()).INDEX();
  if (mfindex < last_mfindex_) {
    // Wrapped
    wrap_count_++;
  }

  last_mfindex_ = mfindex;
  uint64_t wrap_count = wrap_count_;
  // add IST + 1 to get next valid frame. spec 4.11.2.5
  // shift three to convert from 125us microframes to 1ms frames
  return (((wrap_count * (1 << 14)) + mfindex) >> 3) + ist_frames_ + 1;
}

zx_status_t UsbXhci::UsbHciConfigureHub(uint32_t device_id, usb_speed_t speed,
                                        const usb_hub_descriptor_t* desc, bool multi_tt) {
  if (!running_) {
    return ZX_ERR_IO_NOT_PRESENT;
  }
  return RunSynchronously(kPrimaryInterrupter, ConfigureHubAsync(device_id, speed, desc, multi_tt));
}
// Downstream hub port enumeration (USB 2.0 Specification Sections 9.1.2 and 11.12,
// https://www.usb.org/document-library/usb-20-specification; Intel xHCI 1.2 Specification
// Sections 4.3 and 6.2.2,
// https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf)
// issues Enable Slot / Address Device commands and EP0 GET_DESCRIPTOR control transfers. Run via
// RunSynchronously() so enumeration aborts with ZX_ERR_IO_NOT_PRESENT instead of blocking
// indefinitely if the controller halts or stops.
zx_status_t UsbXhci::UsbHciHubDeviceAdded(uint32_t device_id, uint32_t port, usb_speed_t speed) {
  if (!running_) {
    return ZX_ERR_IO_NOT_PRESENT;
  }
  return RunSynchronously(kPrimaryInterrupter, UsbHciHubDeviceAddedAsync(device_id, port, speed));
}

zx_status_t UsbXhci::UsbHciHubDeviceRemoved(uint32_t hub_id, uint32_t port) {
  if (!running_) {
    return ZX_OK;
  }
  if (!IsValidDeviceId(hub_id) || port == 0 || port > std::size(HubInfo{}.port_to_device)) {
    return ZX_ERR_INVALID_ARGS;
  }
  auto hub_state = GetDeviceState(hub_id);
  if (!hub_state) {
    return ZX_OK;
  }
  uint32_t device_id;
  {
    fbl::AutoLock _(&hub_state->transaction_lock());
    // In the case where the hub itself is unplugged,
    // we will likely have torn down the hub state
    // prior to teardown of devices connected to said hub.
    // If this is the case, just return OK.
    // Teardown of child devices will complete asynchronously
    if (hub_state->IsDisconnecting() || !hub_state->GetHubLocked()) {
      return ZX_OK;
    }
    auto device_id_opt =
        std::exchange(hub_state->GetHubLocked()->port_to_device[port - 1], std::nullopt);
    if (!device_id_opt) {
      return ZX_ERR_IO_NOT_PRESENT;
    }
    device_id = *device_id_opt;
  }
  // Per Intel xHCI 1.2 Specification Section 4.6.4 (Disable Slot), removing a downstream port must
  // only disable the child device slot, never the parent hub's own slot.
  if (!IsValidDeviceId(device_id) || device_id == hub_id) {
    return ZX_ERR_IO_NOT_PRESENT;
  }
  auto device_state = GetDeviceState(device_id);
  if (!device_state) {
    return ZX_ERR_IO_NOT_PRESENT;
  }
  // Disconnect the device and every device downstream of it if it is a hub. Disable the slots
  // before completing pending requests. Completing a request unpins its buffers, and the
  // controller may still be accessing them until the slot is disabled.
  // Hold removal state in a shared_ptr because RunSynchronously() may return early on
  // halt/teardown while the scheduled promise still resides on the interrupter executor.
  struct RemovalState {
    std::vector<fbl::RefPtr<DeviceState>> targets;
    std::vector<fbl::RefPtr<DeviceState>> removable_downstream;
    bool root_drained = false;
    zx_status_t root_error = ZX_OK;
  };
  auto removal = std::make_shared<RemovalState>();
  zx_status_t sync_status = RunSynchronously(
      kPrimaryInterrupter,
      fpromise::make_promise([this, device_state, removal] -> fpromise::promise<void, zx_status_t> {
        removal->targets = CollectDisconnectSubtree(device_state);
        std::vector<fpromise::promise<void, zx_status_t>> promises;
        promises.reserve(removal->targets.size());
        for (auto& t : removal->targets) {
          promises.push_back(DisableSlotCommand(*t, t));
        }
        return fpromise::join_promise_vector(std::move(promises))
            .then([this, removal](
                      fpromise::result<std::vector<fpromise::result<void, zx_status_t>>>& results)
                      -> fpromise::result<void, zx_status_t> {
              removal->root_drained =
                  DrainDisabledSubtree(removal->targets, results, "UsbHciHubDeviceRemoved",
                                       &removal->removable_downstream, &removal->root_error);
              return fpromise::ok();
            });
      }).box());

  if (sync_status != ZX_OK) {
    return (!running_ || !SnapshotBusClient().is_valid()) ? ZX_OK : sync_status;
  }

  if (!IsHaltedOrStopping()) {
    if (removal->root_drained && !removal->targets.empty()) {
      if (removal->targets[0]->slot_disabled_.Wait(zx::duration::infinite()) != ZX_OK) {
        Shutdown(ZX_ERR_BAD_STATE);
      }
    }
    for (auto& t : removal->removable_downstream) {
      if (t->slot_disabled_.Wait(zx::duration::infinite()) != ZX_OK) {
        Shutdown(ZX_ERR_BAD_STATE);
      }
    }
  }
  RunUntilIdle();
  if (!running_) {
    return ZX_OK;
  }
  fbl::RefPtr<DeviceState> root =
      (removal->root_drained && !removal->targets.empty()) ? removal->targets[0] : nullptr;
  struct RemoveOutcome {
    zx_status_t status = ZX_OK;
    libsync::Completion done;
  };
  std::shared_ptr<RemoveOutcome> outcome;
  std::shared_ptr<libsync::Completion> enqueued_signal;
  {
    fbl::AutoLock lock(&ddk_interaction_lock_);
    if (ddk_interaction_shutting_down_ || !ddk_interaction_dispatcher_.get()) {
      return ZX_OK;
    }
    if (IsOnDdkInteractionDispatcherLocked()) {
      lock.release();
      return RemoveSubtreeFromBusOnDdkDispatcher(root, removal->removable_downstream,
                                                 removal->root_error);
    }
    outcome = std::make_shared<RemoveOutcome>();
    const zx_status_t post_status =
        async::PostTask(ddk_interaction_dispatcher_.async_dispatcher(),
                        [this, root = std::move(root), removal = std::move(removal), outcome,
                         signal = fit::defer([outcome] { outcome->done.Signal(); })] mutable {
                          outcome->status = RemoveSubtreeFromBusOnDdkDispatcher(
                              root, removal->removable_downstream, removal->root_error);
                        });
    if (post_status != ZX_OK) {
      return ZX_OK;
    }
    enqueued_signal = std::move(hub_remove_enqueued_for_testing_);
  }
  if (enqueued_signal) {
    enqueued_signal->Signal();
  }
  outcome->done.Wait();
  return outcome->status;
}

zx_status_t UsbXhci::UsbHciHubDeviceReset(uint32_t device_id, uint32_t port) {
  return ZX_ERR_NOT_SUPPORTED;
}

zx_status_t UsbXhci::UsbHciResetEndpoint(uint32_t device_id, uint8_t ep_address) {
  return RunSynchronously(kPrimaryInterrupter, UsbHciResetEndpointAsync(device_id, ep_address));
}

fpromise::promise<void, zx_status_t> UsbXhci::UsbHciResetEndpointAsync(uint32_t device_id,
                                                                       uint8_t ep_address) {
  if (!IsValidDeviceId(device_id) || !IsValidEndpointAddress(ep_address)) {
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_INVALID_ARGS);
  }
  auto state = GetDeviceState(device_id);
  if (!state) {
    fdf::error("{} expects that slot is in use. state should not be nullptr", __func__);
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO_NOT_PRESENT);
  }
  struct ResetEndpoint reset_command;
  {
    fbl::AutoLock _(&state->transaction_lock());
    if (state->IsDisconnecting()) {
      return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO_NOT_PRESENT);
    }
    reset_command.set_ENDPOINT(XhciEndpointIndex(ep_address) + 1);
    reset_command.set_SLOT(state->GetSlot());
  }
  auto context = command_ring_.AllocateContext();
  if (!context) {
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_NO_MEMORY);
  }

  TransferRing* ring;
  {
    fbl::AutoLock l(&state->transaction_lock());
    if (ep_address == 0) {
      ring = &state->GetEndpoint().transfer_ring();
    } else {
      ring = &state->GetEndpoint(XhciEndpointIndex(ep_address) - 1).transfer_ring();
    }
    if (!ring->stalled()) {
      return fpromise::make_error_promise<zx_status_t>(ZX_ERR_INVALID_ARGS);
    }
  }
  return SubmitCommand(reset_command, std::move(context))
      .then([=, this](fpromise::result<TRB*, zx_status_t>& result)
                -> fpromise::promise<void, zx_status_t> {
        if (result.is_error()) {
          return fpromise::make_error_promise<zx_status_t>(result.error());
        }
        CommandCompletionEvent* evt = static_cast<CommandCompletionEvent*>(result.value());
        if (evt->CompletionCode() != CommandCompletionEvent::Success) {
          return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO);
        }

        SetTRDequeuePointer cmd;
        cmd.set_ENDPOINT(XhciEndpointIndex(ep_address) + 1);
        cmd.set_SLOT(state->GetSlot());
        auto res = ring->PeekCommandRingControlRegister(CapLength());
        if (res.is_error()) {
          return fpromise::make_error_promise<zx_status_t>(res.error_value());
        }
        cmd.SetPtr(res.value());
        auto context = command_ring_.AllocateContext();
        return SubmitCommand(cmd, std::move(context))
            .then([=](fpromise::result<TRB*, zx_status_t>& result)
                      -> fpromise::result<void, zx_status_t> {
              if (result.is_error()) {
                return fpromise::error(result.error());
              }
              CommandCompletionEvent* evt = static_cast<CommandCompletionEvent*>(result.value());
              if (evt->CompletionCode() != CommandCompletionEvent::Success) {
                return fpromise::error(ZX_ERR_IO);
              }
              fbl::AutoLock l(&state->transaction_lock());
              ring->set_stall(false);
              return fpromise::ok();
            });
      })
      .box();
}

// TODO(https://fxbug.dev/42109966): Either decide what these reset methods should do,
// or get rid of them.
zx_status_t UsbXhci::UsbHciResetDevice(uint32_t hub_address, uint32_t device_id) {
  return ZX_ERR_NOT_SUPPORTED;
}

size_t UsbXhci::UsbHciGetMaxTransferSize(uint32_t device_id, uint8_t ep_address) {
  if (!IsValidDeviceId(device_id) || !IsValidEndpointAddress(ep_address)) {
    return 0;
  }
  auto state = GetDeviceState(device_id);
  if (!state) {
    fdf::error("{} expects that slot is in use. state should not be nullptr", __func__);
    return 0;
  }

  fbl::AutoLock _(&state->transaction_lock());
  if (state->IsDisconnecting()) {
    return 0;
  }
  return SIZE_MAX;
}

zx_status_t UsbXhci::UsbHciCancelAll(uint32_t device_id, uint8_t ep_address) {
  if (!running_) {
    return ZX_ERR_IO_NOT_PRESENT;
  }
  return RunSynchronously(kPrimaryInterrupter, UsbHciCancelAllAsync(device_id, ep_address));
}

fpromise::promise<void, zx_status_t> UsbXhci::UsbHciCancelAllAsync(uint32_t device_id,
                                                                   uint8_t ep_address) {
  if (!IsValidDeviceId(device_id) || !IsValidEndpointAddress(ep_address)) {
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_INVALID_ARGS);
  }
  auto state = GetDeviceState(device_id);
  if (!state) {
    return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO_NOT_PRESENT);
  }

  auto drain_and_complete = [state, ep_address] {
    fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> trbs;
    Endpoint* ep = nullptr;
    {
      fbl::AutoLock _(&state->transaction_lock());
      ep = ep_address ? state->TryGetEndpoint(XhciEndpointIndex(ep_address) - 1)
                      : &state->GetEndpoint();
      if (ep && ep->transfer_ring().active()) {
        trbs = ep->transfer_ring().TakePendingTRBs();
      }
    }
    for (auto& trb : trbs) {
      if (trb.request.has_value()) {
        ep->RequestComplete(ZX_ERR_CANCELED, 0, std::move(*trb.request));
      }
    }
  };

  StopEndpoint stop;
  {
    fbl::AutoLock state_lock(&state->transaction_lock());
    if (state->IsDisconnecting()) {
      state_lock.release();
      if (IsControllerHalted()) {
        drain_and_complete();
      }
      return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO_NOT_PRESENT);
    }
    uint8_t index = static_cast<uint8_t>(XhciEndpointIndex(ep_address) + 1);
    stop.set_ENDPOINT(index);
    stop.set_SLOT(state->GetSlot());
  }
  auto context = command_ring_.AllocateContext();
  return SubmitCommand(stop, std::move(context))
      .then([=, this](fpromise::result<TRB*, zx_status_t>& result)
                -> fpromise::promise<void, zx_status_t> {
        if (result.is_error()) {
          if (result.error() == ZX_ERR_IO_NOT_PRESENT && IsControllerHalted()) {
            drain_and_complete();
            return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
          }
          return fpromise::make_error_promise<zx_status_t>(result.error());
        }
        auto completion_event = static_cast<CommandCompletionEvent*>(result.value());
        auto completion_code = completion_event->CompletionCode();
        zx_status_t status =
            (completion_code == CommandCompletionEvent::Success) ? ZX_OK : ZX_ERR_IO;
        if (status != ZX_OK) {
          return fpromise::make_error_promise<zx_status_t>(status);
        }
        // We can now move everything off of the transfer ring starting at the dequeue pointer
        fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> trbs;
        zx_paddr_t new_ptr_phys = 0;
        Endpoint* ep = nullptr;
        {
          TRB* new_ptr = nullptr;
          fbl::AutoLock _(&state->transaction_lock());
          ep = ep_address ? &state->GetEndpoint(XhciEndpointIndex(ep_address) - 1)
                          : &state->GetEndpoint();
          if (state->IsDisconnecting()) {
            if (ep->transfer_ring().active()) {
              trbs = ep->transfer_ring().TakePendingTRBs();
            }
          } else {
            auto& transfer_ring = ep->transfer_ring();
            if (!transfer_ring.active()) {
              return fpromise::make_error_promise<zx_status_t>(ZX_ERR_IO_NOT_PRESENT);
            }
            trbs = transfer_ring.TakePendingTRBs();
            for (auto& trb : trbs) {
              new_ptr = trb.trb;
              auto control = Control::FromTRB(trb.trb);
              control.set_Cycle(!control.Cycle());
            }
            if (new_ptr) {
              new_ptr_phys = transfer_ring.VirtToPhys(new_ptr + 1);
            }
          }
        }
        for (auto& trb : trbs) {
          if (trb.request.has_value()) {
            ep->RequestComplete(ZX_ERR_CANCELED, 0, std::move(*trb.request));
          }
        }

        if (!new_ptr_phys) {
          return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
        }

        // It's possible that the dequeue pointer was in the middle of a multi-TRB TD when we
        // stopped. If this is the case, we need to adjust the dequeue pointer to point to the
        // index of the first TRB that we know about.
        SetTRDequeuePointer cmd;
        cmd.set_ENDPOINT(XhciEndpointIndex(ep_address) + 1);
        cmd.set_SLOT(state->GetSlot());
        cmd.ptr = new_ptr_phys;
        auto context = command_ring_.AllocateContext();
        return SubmitCommand(cmd, std::move(context))
            .then([=](fpromise::result<TRB*, zx_status_t>& result)
                      -> fpromise::result<void, zx_status_t> {
              if (result.is_error()) {
                return fpromise::error(result.error());
              }
              auto completion_event = static_cast<CommandCompletionEvent*>(result.value());
              auto completion_code = completion_event->CompletionCode();
              bool command_success = completion_code == CommandCompletionEvent::Success;
              zx_status_t status = command_success ? ZX_OK : ZX_ERR_IO;
              if (status != ZX_OK) {
                return fpromise::error(status);
              }
              return fpromise::ok();
            });
      })
      .box();
}

size_t UsbXhci::UsbHciGetRequestSize() { return Request::RequestSize(sizeof(usb_request_t)); }

void UsbXhci::Shutdown(zx_status_t status) {
  if (!running_.exchange(false)) {
    return;
  }
  if (mmio_.has_value()) {
    HaltController("Shutdown");
  }
  if (status != ZX_OK) {
    // If we're shutting down due to an error (not just regular unbind)
    // ensure that we remove the device.
    fdf::error("Internal xhci error, shutting down with error: {}", zx_status_get_string(status));
    if (controller_.is_valid()) {
      auto result = controller_->Remove();
      if (!result.ok()) {
        fdf::error("Failed to remove node {}", result.FormatDescription().c_str());
      }
    }
  }
}

zx_status_t UsbXhci::InitQuirks() {
  fuchsia_hardware_pci::wire::DeviceInfo info;
  auto status = pci_.GetDeviceInfo(&info);
  if (status == ZX_ERR_PEER_CLOSED) {
    // Reset.
    pci_ = {};
  }
  if (status != ZX_OK) {
    return status;
  }
  if ((info.vendor_id == 0x1033) && (info.device_id == 0x194)) {
    qemu_quirk_ = true;
  }
  if ((info.vendor_id) == 0x8086 && (info.device_id == 0x8C31)) {
    // TODO(bbosak): Implement stub EHCI driver so we can properly
    // do the handoff in case the BIOS is managing a device on EHCI.
    // Quirk for some older Intel chipsets
    // Switch ports from EHCI to XHCI.
    uint32_t ports_available;
    pci_.ReadConfig32(0xdc, &ports_available);
    if (ports_available) {
      pci_.WriteConfig32(0xd8, ports_available);
    }
    // Route power and data lines for USB 2.0 ports
    pci_.ReadConfig32(0xd4, &ports_available);
    if (ports_available) {
      pci_.WriteConfig32(0xD0, ports_available);
    }
    // Handoff takes 5 seconds if we're contending with the EHCI controller.
    // (have to wait for enumeration to time out)
    sleep(5);
  }

  return ZX_OK;
}

zx_status_t UsbXhci::InitPci() {
  // Perform vendor-specific workarounds.
  auto status = InitQuirks();
  if (status != ZX_OK) {
    return status;
  }
  // PCIe interface supports cache snooping
  has_coherent_cache_ = true;
  // Initialize MMIO
  std::optional<fdf::MmioBuffer> buffer;
  status = pci_.MapMmio(0, ZX_CACHE_POLICY_UNCACHED_DEVICE, &buffer);
  if (status != ZX_OK) {
    return status;
  }
  mmio_ = std::move(*buffer);
  irq_count_ =
      std::min(kMaxInterrupters,
               static_cast<uint16_t>(HCSPARAMS1::Get().ReadFrom(&mmio_.value()).MaxIntrs()));

  // Make sure irq_count_ doesn't exceed supported max PCI IRQs.
  fuchsia_hardware_pci::wire::InterruptModes modes{};
  pci_.GetInterruptModes(&modes);
  uint32_t mode_irq_max = std::max(static_cast<uint16_t>(modes.msi_count), modes.msix_count);
  irq_count_ = std::min(irq_count_, static_cast<uint16_t>(mode_irq_max));
  status = pci_.ConfigureInterruptMode(irq_count_, /*out_mode=*/nullptr);
  if (status != ZX_OK) {
    return status;
  }
  fbl::AllocChecker ac;
  interrupters_ = fbl::MakeArray<Interrupter>(&ac, irq_count_);
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  for (uint16_t i = 0; i < irq_count_; i++) {
    status = pci_.MapInterrupt(i, &interrupter(i).GetIrq());
    if (status != ZX_OK) {
      return status;
    }
  }
  status = pci_.SetBusMastering(true);
  if (status != ZX_OK) {
    return status;
  }
  return ZX_OK;
}

zx_status_t UsbXhci::InitMmio() {
  if (!pdev_.is_valid()) {
    return ZX_ERR_IO_INVALID;
  }
  zx::result mmio = pdev_.MapMmio(0);
  if (mmio.is_error()) {
    fdf::error("Failed to map mmio: {}", mmio);
    return mmio.status_value();
  }
  mmio_ = std::move(*mmio);
  irq_count_ = static_cast<uint16_t>(HCSPARAMS1::Get().ReadFrom(&mmio_.value()).MaxIntrs());
  if (zx::result<fdf::PDev::DeviceInfo> dev_info = pdev_.GetDeviceInfo(); dev_info.is_ok()) {
    irq_count_ =
        std::max<uint16_t>(1, std::min(irq_count_, static_cast<uint16_t>(dev_info->irq_count)));
  } else {
    irq_count_ = 1;
  }
  fbl::AllocChecker ac;
  interrupters_ = fbl::MakeArray<Interrupter>(&ac, irq_count_);
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  for (uint16_t i = 0; i < irq_count_; i++) {
    zx::result interrupt = pdev_.GetInterrupt(i, 0);
    if (interrupt.is_error()) {
      fdf::error("Failed to get interrupt {}: {}", i, interrupt);
      return interrupt.status_value();
    }
    interrupter(i).GetIrq() = std::move(interrupt.value());
  }
  return ZX_OK;
}

void UsbXhci::ParseSupportedProtocol() {
  HCCPARAMS1 hcc = HCCPARAMS1::Get().ReadFrom(&mmio_.value());
  if (hcc.ReadFrom(&mmio_.value()).xECP()) {
    XECP current = XECP::Get(hcc).ReadFrom(&mmio_.value());
    while (true) {
      if (current.ID() == XECP::SupportedProtocol) {
        auto word0 = CapabilityRegWord0::Get(current).ReadFrom(&mmio_.value());
        auto word2 = CapabilityRegWord2::Get(current).ReadFrom(&mmio_.value());

        fdf::info(
            "xhci: Supported protocol info: Revision={}.{} USB{} port_offset={} port_count={}",
            word0.MAJOR_REVISION(), word0.MINOR_REVISION(), word0.MAJOR_REVISION(),
            word2.PortOffset(), word2.PortCount());
      }
      if (!current.NEXT()) {
        break;
      }
      current = current.Next().ReadFrom(&mmio_.value());
    }
  }
}

void UsbXhci::BiosHandoff() {
  HCCPARAMS1 hcc = HCCPARAMS1::Get().ReadFrom(&mmio_.value());
  if (hcc.ReadFrom(&mmio_.value()).xECP()) {
    XECP current = XECP::Get(hcc).ReadFrom(&mmio_.value());
    while (true) {
      if (current.ID() == XECP::UsbLegacySupport) {
        // Check is BIOS owns the host controller. If so, signal BIOS.
        if (current.reg_value() & kBiosOwned) {
          current.set_reg_value(current.reg_value() | kOSOwned).WriteTo(&mmio_.value());

          // Wait for control transfer.
          current = current.ReadFrom(&mmio_.value());
          while (current.reg_value() & kBiosOwned) {
            current = current.ReadFrom(&mmio_.value());
            usleep(10);
          };
        }
      }
      if (!current.NEXT()) {
        break;
      }
      current = current.Next().ReadFrom(&mmio_.value());
    }
  }
}

void UsbXhci::ResetController() {
  USBCMD::Get(cap_length_).ReadFrom(&mmio_.value()).set_ENABLE(0).WriteTo(&mmio_.value());
  while (!USBSTS::Get(cap_length_).ReadFrom(&mmio_.value()).HCHalted()) {
    zx::nanosleep(zx::deadline_after(zx::msec(1)));
  }
  while (USBSTS::Get(cap_length_).ReadFrom(&mmio_.value()).CNR()) {
    zx::nanosleep(zx::deadline_after(zx::msec(1)));
  }
  USBCMD::Get(cap_length_).ReadFrom(&mmio_.value()).set_RESET(1).WriteTo(&mmio_.value());
  while (USBCMD::Get(cap_length_).ReadFrom(&mmio_.value()).RESET()) {
    zx::nanosleep(zx::deadline_after(zx::msec(1)));
  }
  while (USBSTS::Get(cap_length_).ReadFrom(&mmio_.value()).CNR()) {
    zx::nanosleep(zx::deadline_after(zx::msec(1)));
  }
}

zx::result<> UsbXhci::Init(std::unique_ptr<dma_buffer::BufferFactory> buffer_factory) {
  buffer_factory_ = std::move(buffer_factory);

  // Initialize either the PCI or MMIO structures first
  zx_status_t status;
  if (pci_.is_valid()) {
    status = InitPci();
    if (status != ZX_ERR_PEER_CLOSED && status != ZX_OK) {
      fdf::error("PCI initialization failed with: {}", zx_status_get_string(status));
      return zx::error(status);
    }
  }
  // Because pci_.is_valid() may have changed in InitQuirks().
  if (!pci_.is_valid()) {
    status = InitMmio();
    if (status != ZX_OK) {
      fdf::error("MMIO initialization failed with: {}", zx_status_get_string(status));
      return zx::error(status);
    }
  }
  // Perform the BIOS handoff if necessary
  BiosHandoff();

  // Read Supported Protocol Capabilities if available.
  ParseSupportedProtocol();

  // At startup the device is in an unknown state
  // Reset the xHCI to place everything in its well-defined
  // initial state.
  uint8_t cap_length = CapLength::Get().ReadFrom(&mmio_.value()).Length();
  cap_length_ = cap_length;
  // Perform xHCI reset process
  ResetController();
  // Finish HCI initialization
  status = HciFinalize();
  if (status != ZX_OK) {
    fdf::error("xHCI initialization failed with {}", zx_status_get_string(status));
    return zx::error(status);
  }
  status = CreateNode();
  if (status != ZX_OK) {
    fdf::error("Failed to create node {}", zx_status_get_string(status));
    return zx::error(status);
  }
  return zx::ok();
}

zx_status_t UsbXhci::HciFinalize() {
  hcc_ = HCCPARAMS1::Get().ReadFrom(&mmio_.value());
  HCSPARAMS1 hcsparams1 = HCSPARAMS1::Get().ReadFrom(&mmio_.value());

  // Initialize Inspect values
  HciVersion hci_version = HciVersion::Get().ReadFrom(&mmio_.value());
  inspect_.Init(inspector_->root(), hci_version.reg_value(), hcsparams1, hcc_);

  // Reset Warm Reset Change (WRC) bit if necessary (see Table 5-27, bit 19 in Section 5.4.8,
  // xHCI specification). This is done to acknowledge any warm reset done during bootup.
  for (uint16_t i = 0; i < hcsparams1.MaxPorts(); i++) {
    auto sc = PORTSC::Get(cap_length_, i + 1).ReadFrom(&mmio_.value());
    if (sc.WRC()) {
      sc.set_WRC(sc.WRC()).WriteTo(&mmio_.value());
    }
  }

  zx_status_t status;
  is_32bit_ = !hcc_.AC64();
  params_ = hcsparams1;
  CONFIG::Get(cap_length_)
      .ReadFrom(&mmio_.value())
      .set_MaxSlotsEn(hcsparams1.MaxSlots())
      .WriteTo(&mmio_.value());
  {
    zx::bti bti;
    if (pci_.is_valid()) {
      if (status = pci_.GetBti(0, &bti); status != ZX_OK) {
        fdf::error("pci_.GetBti(): {}", zx_status_get_string(status));
        return ZX_ERR_INTERNAL;
      }
    } else {
      zx::result result = pdev_.GetBti(0);
      if (result.is_error()) {
        fdf::error("Failed to get bti: {}", result);
        return result.status_value();
      }
      bti = std::move(result.value());
    }
    bti_ = std::move(bti);
  }
  // ResetController() has already halted and reset the hardware, so any DMA pages quarantined by
  // a previous driver instance (e.g. after a halt timeout) can now be safely reclaimed.
  if (status = bti_.release_quarantine(); status != ZX_OK) {
    fdf::error("Failed to release BTI quarantine: {}", zx_status_get_string(status));
    return status;
  }
  uint32_t page_size = USB_PAGESIZE::Get(cap_length_).ReadFrom(&mmio_.value()).PageSize() << 12;
  page_size_ = page_size;
  // TODO(bbosak): Correct this to use variable alignment when we get kernel
  // support for this.
  if (page_size != zx_system_get_page_size()) {
    fdf::error("xHC page size differs from platform page size");
    return ZX_ERR_INTERNAL;
  }
  uint32_t align_log2 = 0;

  status = buffer_factory_->CreatePaged(bti_, zx_system_get_page_size(),
                                        dma_buffer::CacheOptions::kDisabled, &dcbaa_buffer_);
  if (status != ZX_OK) {
    fdf::error("buffer_factory_->CreatePaged(): {}", zx_status_get_string(status));
    return ZX_ERR_INTERNAL;
  }
  if (is_32bit_ && (dcbaa_buffer_->phys()[0] >= UINT32_MAX)) {
    fdf::error("xHC using 32bit mode but dcbaa buffer address overflows 32bits");
    return ZX_ERR_INTERNAL;
  }
  dcbaa_ = static_cast<uint64_t*>(dcbaa_buffer_->virt());
  fbl::AllocChecker ac;
  HCSPARAMS2 hcsparams2 = HCSPARAMS2::Get().ReadFrom(&mmio_.value());
  auto ist = hcsparams2.IST();
  ist_frames_ = (ist & 0x8) ? /* frames */ (ist & 0x7)
                            : /* microframes */ RoundUp(ist & 0x7, kMicroframesPerFrame);
  RuntimeRegisterOffset offset = RuntimeRegisterOffset::Get().ReadFrom(&mmio_.value());
  runtime_offset_ = offset;
  // Per Intel xHCI 1.2 Section 5.3.4 (Table 5-11), Max Scratchpad Buffers is the 10-bit count
  // formed by (Max Scratchpad Bufs Hi << 5) | Max Scratchpad Bufs Lo. When positive, DCBAA[0]
  // points to a Scratchpad Buffer Array populated with all 0..max_scratchpad_buffers - 1
  // page-sized buffer addresses (Sections 4.20 and 6.6); when 0, DCBAA[0] shall be 0 (Section 6.1,
  // Table 6-1). See:
  // https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf
  uint32_t max_scratchpad_buffers =
      hcsparams2.MAX_SCRATCHPAD_BUFFERS_LOW() | (hcsparams2.MAX_SCRATCHPAD_BUFFERS_HIGH() << 5);
  if (max_scratchpad_buffers > 0) {
    scratchpad_buffers_ =
        fbl::MakeArray<std::unique_ptr<dma_buffer::ContiguousBuffer>>(&ac, max_scratchpad_buffers);
    if (!ac.check()) {
      fdf::error("fbl::MakeArray<std::unique_ptr<dma_buffer::ContiguousBuffer>> allocation fails");
      return ZX_ERR_NO_MEMORY;
    }
    if (fbl::round_up(max_scratchpad_buffers * sizeof(uint64_t), zx_system_get_page_size()) >
        zx_system_get_page_size()) {
      // We can't create multi-page contiguously physical uncached buffers.
      // This is presently not supported in the kernel.
      // TODO(b/409558323): The above comment is no longer true.
      fdf::error("physically-contiguous buffer exceeds system pagesize");
      return ZX_ERR_NOT_SUPPORTED;
    }
    status = buffer_factory_->CreatePaged(bti_, zx_system_get_page_size(),
                                          dma_buffer::CacheOptions::kDisabled,
                                          &scratchpad_buffer_array_);
    if (status != ZX_OK) {
      fdf::error("buffer_factory_->CreatePaged(scratchpad_buffer_array): {}",
                 zx_status_get_string(status));
      return ZX_ERR_INTERNAL;
    }
    if (is_32bit_ && (scratchpad_buffer_array_->phys()[0] >= UINT32_MAX)) {
      fdf::error("xHC using 32bit mode but scratchpad buffer[0] address overflows 32bits");
      return ZX_ERR_INTERNAL;
    }
    uint64_t* scratchpad_buffer_array = static_cast<uint64_t*>(scratchpad_buffer_array_->virt());
    for (size_t i = 0; i < max_scratchpad_buffers; i++) {
      status = buffer_factory_->CreateContiguous(
          bti_, page_size, align_log2, dma_buffer::CacheOptions::kEnabled, &scratchpad_buffers_[i]);
      if (status != ZX_OK) {
        fdf::error("buffer_factory_->CreateContiguous(buffer[{}]): {}", i,
                   zx_status_get_string(status));
        return ZX_ERR_INTERNAL;
      }
      if (is_32bit_ && (scratchpad_buffers_[i]->phys() >= UINT32_MAX)) {
        fdf::error("xHC using 32bit mode but scratchpad buffer[{}] address overflows 32bits", i);
        return ZX_ERR_INTERNAL;
      }
      scratchpad_buffer_array[i] = scratchpad_buffers_[i]->phys();
    }
    dcbaa_[0] = scratchpad_buffer_array_->phys()[0];
  } else {
    dcbaa_[0] = 0;
  }
  max_slots_ = hcsparams1.MaxSlots();
  slot_size_bytes_ = hcc_.CSZ() == 1 ? 64 : 32;
  auto device_states = fbl::MakeArray<fbl::RefPtr<DeviceState>>(&ac, max_slots_);
  if (!ac.check()) {
    fdf::error("fbl::MakeArray<DeviceState> allocation fails");
    return ZX_ERR_NO_MEMORY;
  }
  InitDeviceState(std::move(device_states));
  port_state_ = fbl::MakeArray<PortState>(&ac, hcsparams1.MaxPorts());
  if (!ac.check()) {
    fdf::error("fbl::MakeArray<PortState> allocation fails");
    return ZX_ERR_NO_MEMORY;
  }
  DataMemoryBarrier();
  DCBAAP::Get(cap_length_).FromValue(0).set_PTR(dcbaa_buffer_->phys()[0]).WriteTo(&mmio_.value());
  // Initialize command ring
  doorbell_offset_ = DoorbellOffset::Get().ReadFrom(&mmio_.value());
  // Interrupt moderation interval == 30 microseconds (optimal value derived from scheduler trace)
  // TODO: Change this based on P state (performance states) for power management
  for (uint16_t i = 0; i < irq_count_; i++) {
    status = interrupter(i).Init(i, page_size, &mmio_.value(), offset, 1 << hcsparams2.ERST_MAX(),
                                 doorbell_offset_, this, hcc_, dcbaa_);
    if (status != ZX_OK) {
      fdf::error("interrupter({}).Init(): {}", i, zx_status_get_string(status));
      return ZX_ERR_INTERNAL;
    }
  }
  status =
      command_ring_.Init(page_size, &bti_, &interrupter(0).ring(), is_32bit_, &mmio_.value(), this);
  if (status != ZX_OK) {
    fdf::error("command_ring_.Init(): {}", zx_status_get_string(status));
    return ZX_ERR_INTERNAL;
  }
  CRCR cr = command_ring_.phys(cap_length_);
  cr.WriteTo(&mmio_.value());

  // Initialize all interrupters.
  // TODO: For optimization, we could demand allocate interrupters and not start all interrupters
  // in the beginning.
  for (uint16_t i = 0; i < irq_count_; i++) {
    if (status = interrupter(i).Start(offset, mmio_.value().View(0)); status != ZX_OK) {
      fdf::error("interrupter({}).Start(): {}", i, zx_status_get_string(status));
      return ZX_ERR_INTERNAL;
    }
  }
  return ZX_OK;
}

zx_status_t UsbXhci::CreateNode() {
  {
    auto result = outgoing()->AddService<fuchsia_hardware_usb_hci::UsbHciService>(
        fuchsia_hardware_usb_hci::UsbHciService::InstanceHandler({
            .device =
                bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                        fidl::kIgnoreBindingClosure),
        }));
    if (result.is_error()) {
      fdf::error("Failed to add UsbHci service {}", result);
      return result.status_value();
    }
  }

  std::vector<fuchsia_driver_framework::NodeProperty2> props{banjo_server_.property()};
  compat::DeviceServer::BanjoConfig banjo_config;
  banjo_config.callbacks[ZX_PROTOCOL_USB_HCI] = banjo_server_.callback();
  {
    zx::result<> result =
        compat_server_.Initialize(incoming_, outgoing(), node_name_, kDeviceName,
                                  compat::ForwardMetadata::All(), std::move(banjo_config));
    if (result.is_error()) {
      fdf::error("Failed to init compat server {}", result);
      return result.error_value();
    }
  }
  auto offers = compat_server_.CreateOffers2();
  offers.push_back(fdf::MakeOffer2<fuchsia_hardware_usb_hci::UsbHciService>());
  auto result = AddChild(kDeviceName, props, offers);
  if (result.is_error()) {
    fdf::error("Failed to add child {}", result);
    return result.error_value();
  }
  controller_.Bind(std::move(*result));

  return ZX_OK;
}

void UsbXhci::ConnectToEndpoint(ConnectToEndpointRequest& request,
                                ConnectToEndpointCompleter::Sync& completer) {
  if (!IsValidDeviceId(request.device_id())) {
    completer.Reply(fit::as_error(ZX_ERR_NOT_FOUND));
    return;
  }
  if (!IsValidEndpointAddress(request.ep_addr()) || request.ep_addr() == 0) {
    completer.Reply(fit::as_error(ZX_ERR_NOT_FOUND));
    return;
  }
  auto state = GetDeviceState(request.device_id());
  if (!state) {
    fdf::error("{} expects that slot is in use. state should not be nullptr", __func__);
    completer.Reply(fit::as_error(ZX_ERR_IO_NOT_PRESENT));
    return;
  }
  uint8_t index = XhciEndpointIndex(request.ep_addr());
  fbl::AutoLock _(&state->transaction_lock());
  if (state->IsDisconnecting()) {
    completer.Reply(fit::as_error(ZX_ERR_IO_NOT_PRESENT));
    return;
  }
  auto& ep = state->GetEndpoint(index - 1);
  ep.Connect(dispatcher(), std::move(request.ep()), state);
  completer.Reply(fit::ok());
}

TRBPromise UsbXhci::SubmitCommand(const TRB& command, std::unique_ptr<TRBContext> trb_context) {
  if (!running_ || !mmio_.has_value() ||
      !USBCMD::Get(cap_length_).ReadFrom(&mmio_.value()).ENABLE() ||
      USBSTS::Get(cap_length_).ReadFrom(&mmio_.value()).HCHalted()) {
    return fpromise::make_result_promise(
               fpromise::result<TRB*, zx_status_t>(fpromise::error(ZX_ERR_IO_NOT_PRESENT)))
        .box();
  }
  fpromise::bridge<TRB*, zx_status_t> bridge;
  trb_context->completer = std::move(bridge.completer);
  zx_status_t status = command_ring_.AddTRB(command, std::move(trb_context));
  if (status != ZX_OK) {
    return fpromise::make_result_promise(
               fpromise::result<TRB*, zx_status_t>(fpromise::error(status)))
        .box();
  }
  // Ring the doorbell
  DOORBELL::Get(doorbell_offset_, 0).FromValue(0).WriteTo(&mmio_.value());
  return bridge.consumer.promise().box();
}

// Initialize xHCI Inspect node and values.
void Inspect::Init(inspect::Node& parent, uint16_t hci_version_in, HCSPARAMS1& hcs1,
                   HCCPARAMS1& hcc1) {
  root = parent.CreateChild("usb-xhci");

  hci_version = root.CreateUint("hci_version", hci_version_in);
  max_device_slots = root.CreateUint("max_device_slots", hcs1.MaxSlots());
  max_interrupters = root.CreateUint("max_interrupters", hcs1.MaxIntrs());
  max_ports = root.CreateUint("max_ports", hcs1.MaxPorts());
  has_64_bit_addressing = root.CreateBool("has_64_bit_addressing", hcc1.AC64());
  context_size_bytes = root.CreateUint("context_size_bytes", hcc1.CSZ() == 1 ? 64 : 32);
}

zx::result<> UsbXhci::Start(fdf::DriverContext context) {
  node_name_ = std::string(context.node_name().value_or(""));
  inspector_ = context.CreateInspector(this);
  config_ = context.take_config<xhci_config::Config>();
  incoming_ = context.take_incoming();
  // Run PostCallback tasks on a dedicated synchronized dispatcher with kAllowSyncCalls because
  // bus_.AddDevice() synchronously initializes the new UsbDevice and binds child interface drivers,
  // which in turn make synchronous FIDL calls (e.g. UsbHci::ConnectToEndpoint) serviced on this
  // driver's default dispatcher(). Running PostCallback on dispatcher() would deadlock when
  // usb-bus and xhci are colocated.
  auto ddk_dispatcher_result = fdf::SynchronizedDispatcher::Create(
      fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "xhci-ddk-interaction",
      [this](fdf_dispatcher_t*) { ddk_interaction_shutdown_.Signal(); });
  if (ddk_dispatcher_result.is_error()) {
    fdf::error("Failed to create xhci-ddk-interaction dispatcher: {}", ddk_dispatcher_result);
    return ddk_dispatcher_result.take_error();
  }
  {
    fbl::AutoLock _(&ddk_interaction_lock_);
    ddk_interaction_started_ = true;
    ddk_interaction_shutting_down_ = false;
    ddk_interaction_dispatcher_ = std::move(*ddk_dispatcher_result);
  }

  zx::result pdev_result =
      incoming_->Connect<fuchsia_hardware_platform_device::Service::Device>("pdev");
  if (pdev_result.is_ok() && pdev_result->is_valid()) {
    pdev_ = fdf::PDev{std::move(*pdev_result)};
  } else {
    // A device doesn't have to have a PDEV. It might use PCI instead.
    if (pdev_result.error_value() != ZX_ERR_NOT_FOUND) {
      fdf::error("UsbXhci::Init: could not get platform device protocol: {}", pdev_result);
      return zx::error(ZX_ERR_NOT_SUPPORTED);
    }
  }
  zx::result pci_result = incoming_->Connect<fuchsia_hardware_pci::Service::Device>("pdev");
  if (pci_result.is_ok()) {
    pci_ = fdf::Pci(std::move(*pci_result));
  }

  if (!(pci_.is_valid() || pdev_.is_valid())) {
    return zx::error(ZX_ERR_IO_INVALID);
  }

#ifndef TEST_SKIP_INIT
  zx::result<> result = Init(dma_buffer::CreateBufferFactory());
  if (result.is_error()) {
    fdf::error("Init failed {}", result);
    return result.take_error();
  }
#endif
  return zx::ok();
}

}  // namespace usb_xhci

FUCHSIA_DRIVER_EXPORT2(usb_xhci::UsbXhci);
