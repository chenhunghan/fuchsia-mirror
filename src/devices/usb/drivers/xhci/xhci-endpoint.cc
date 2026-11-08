// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/xhci/xhci-endpoint.h"

#include <lib/fit/defer.h>

#include "src/devices/usb/drivers/xhci/usb-xhci.h"

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace usb_xhci {

Endpoint::Endpoint(UsbXhci* hci, uint32_t device_id, uint8_t address)
    : usb::EndpointServer(hci->bti(), address, usb::ScatterGatherSupport::kSupported),
      hci_(hci),
      device_id_(device_id) {
  ZX_DEBUG_ASSERT(hci_ != nullptr);
}

namespace {

inline usb_setup_t ToBanjo(fdescriptor::UsbSetup setup) {
  return usb_setup_t{
      .bm_request_type = setup.bm_request_type(),
      .b_request = setup.b_request(),
      .w_value = setup.w_value(),
      .w_index = setup.w_index(),
      .w_length = setup.w_length(),
  };
}

}  // namespace

struct Endpoint::LockedQueue {
  static zx_status_t QueueRequestLocked(Endpoint& ep, DeviceState& state,
                                        std::optional<usb::RequestVariant>& request)
      __TA_REQUIRES(state.transaction_lock());
  static zx_status_t ControlRequestQueueLocked(Endpoint& ep, DeviceState& state,
                                               std::optional<usb::RequestVariant>& request)
      __TA_REQUIRES(state.transaction_lock());
  static zx_status_t NormalRequestQueueLocked(Endpoint& ep, DeviceState& state,
                                              std::optional<usb::RequestVariant>& request)
      __TA_REQUIRES(state.transaction_lock());
};

zx_status_t Endpoint::Init(EventRing* event_ring, fdf::MmioBuffer* mmio) {
  if (!hci_->Running()) {
    return ZX_ERR_BAD_STATE;
  }
  return transfer_ring_.Init(hci_->GetPageSize(), hci_->bti(), event_ring,
                             hci_->Is32BitController(), mmio, hci_);
}

Endpoint::~Endpoint() { transfer_ring_.DeinitIfActive(); }

void Endpoint::Connect(async_dispatcher_t* dispatcher,
                       fidl::ServerEnd<fuchsia_hardware_usb_endpoint::Endpoint> server_end,
                       fbl::RefPtr<DeviceState> state) {
  if (state) {
    fbl::AutoLock _(&binding_lock_);
    binding_refs_.push_back(std::move(state));
  } else if (hci_->GetDeviceState()) {
    auto dev_state = hci_->GetDeviceState()[device_id_];
    if (dev_state) {
      fbl::AutoLock _(&binding_lock_);
      binding_refs_.push_back(std::move(dev_state));
    }
  }
  usb::EndpointServer::Connect(dispatcher, std::move(server_end));
}

void Endpoint::OnUnbound(fidl::UnbindInfo info,
                         fidl::ServerEnd<fuchsia_hardware_usb_endpoint::Endpoint> server_end) {
  // Hold the DeviceState reference on the stack until OnUnbound finishes so that `this`
  // (owned by DeviceState) remains valid for the entire teardown callback even if
  // UsbXhci::DeviceOffline already dropped UsbXhci's reference in device_state_.
  fbl::RefPtr<DeviceState> state_ref;
  {
    fbl::AutoLock _(&binding_lock_);
    if (!binding_refs_.empty()) {
      state_ref = std::move(binding_refs_.back());
      binding_refs_.pop_back();
    }
  }

  bool disconnecting = true;
  if (state_ref) {
    fbl::AutoLock _(&state_ref->transaction_lock());
    disconnecting = state_ref->IsDisconnecting() || !state_ref->GetSlot();
  }

  if (hci_->Running() && !disconnecting) {
    auto status = hci_->UsbHciCancelAll(device_id_, ep_addr());
    if (status != ZX_OK && status != ZX_ERR_IO_NOT_PRESENT) {
      fdf::error("Could not cancel all {}", zx_status_get_string(status));
    }

    status = hci_->RunSynchronously(kPrimaryInterrupter,
                                    hci_->UsbHciDisableEndpoint(device_id_, ep_addr()));
    if (status != ZX_OK && status != ZX_ERR_IO_NOT_PRESENT) {
      fdf::error("Could not disable endpoint {}", zx_status_get_string(status));
    }
  }

  usb::EndpointServer::OnUnbound(info, std::move(server_end));
}
void Endpoint::QueueRequests(QueueRequestsRequest& request,
                             QueueRequestsCompleter::Sync& completer) {
  if (!hci_->Running()) {
    for (auto& req : request.req()) {
      RequestComplete(ZX_ERR_BAD_STATE, 0, usb::FidlRequest{std::move(req)});
    }
    return;
  }
  auto state = hci_->GetDeviceState() ? hci_->GetDeviceState()[device_id_] : nullptr;
  if (!state) {
    for (auto& req : request.req()) {
      RequestComplete(ZX_ERR_IO_NOT_PRESENT, 0, usb::FidlRequest{std::move(req)});
    }
    return;
  }
  for (auto& req : request.req()) {
    QueueRequest(usb::FidlRequest{std::move(req)});
  }
  if (!hci_->Running() || !hci_->GetDeviceState() || !hci_->GetDeviceState()[device_id_]) {
    return;
  }
  uint8_t slot = 0;
  {
    fbl::AutoLock _(&state->transaction_lock());
    if (state->IsDisconnecting() || !state->GetSlot()) {
      return;
    }
    slot = state->GetSlot();
  }
  uint8_t index = static_cast<uint8_t>(XhciEndpointIndex(ep_addr()) - 1);
  hci_->RingDoorbell(slot, ep_addr() ? 2 + index : 1);
}

void Endpoint::CancelAll(CancelAllCompleter::Sync& completer) {
  if (!hci_->Running()) {
    completer.Reply(fit::as_error(ZX_ERR_BAD_STATE));
    return;
  }
  auto state = hci_->GetDeviceState() ? hci_->GetDeviceState()[device_id_] : nullptr;
  if (!state) {
    completer.Reply(fit::as_error(ZX_ERR_IO_NOT_PRESENT));
    return;
  }
  {
    fbl::AutoLock _(&state->transaction_lock());
    if (state->IsDisconnecting() || !state->GetSlot()) {
      completer.Reply(fit::as_error(ZX_ERR_IO_NOT_PRESENT));
      return;
    }
  }
  auto status = hci_->UsbHciCancelAll(device_id_, ep_addr());
  if (status != ZX_OK) {
    completer.Reply(fit::as_error(status));
    return;
  }
  completer.Reply(fit::ok());
}

void Endpoint::QueueRequest(usb::RequestVariant request) {
  if (!hci_->Running()) {
    RequestComplete(ZX_ERR_BAD_STATE, 0, std::move(request));
    return;
  }
  auto state = hci_->GetDeviceState() ? hci_->GetDeviceState()[device_id_] : nullptr;
  if (!state) {
    RequestComplete(ZX_ERR_IO_NOT_PRESENT, 0, std::move(request));
    return;
  }
  std::optional<usb::RequestVariant> pending_request(std::move(request));
  zx_status_t status = ZX_OK;
  {
    fbl::AutoLock transaction_lock(&state->transaction_lock());
    status = LockedQueue::QueueRequestLocked(*this, *state, pending_request);
  }
  if (status != ZX_OK) {
    RequestComplete(status, 0, std::move(*pending_request));
  }
}

zx_status_t Endpoint::LockedQueue::QueueRequestLocked(Endpoint& ep, DeviceState& state,
                                                      std::optional<usb::RequestVariant>& request) {
  if (!state.GetSlot() || state.IsDisconnecting()) {
    return ZX_ERR_IO_NOT_PRESENT;
  }
  if (unlikely(ep.ep_addr() == 0)) {
    return ControlRequestQueueLocked(ep, state, request);
  }
  return NormalRequestQueueLocked(ep, state, request);
}

zx_status_t Endpoint::LockedQueue::ControlRequestQueueLocked(
    Endpoint& ep, DeviceState& state, std::optional<usb::RequestVariant>& request) {
  if (state.IsDisconnecting() || !state.GetSlot()) {
    return ZX_ERR_IO_NOT_PRESENT;
  }
  if (ep.transfer_ring_.stalled()) {
    return ZX_ERR_IO_REFUSED;
  }
  auto context = ep.transfer_ring_.AllocateContext();
  if (!context) {
    return ZX_ERR_NO_MEMORY;
  }
  TransferRing::State transaction;
  TRB* setup;
  zx_status_t status = ep.transfer_ring_.AllocateTRB(&setup, &transaction);
  if (status != ZX_OK) {
    ep.transfer_ring_.Restore(transaction);
    return status;
  }

  context->request.emplace(std::move(*request));
  request.reset();
  UsbRequestState pending_transfer;
  pending_transfer.context = std::move(context);
  pending_transfer.setup = setup;
  pending_transfer.transaction = transaction;
  auto rollback = fit::defer([&] {
    ep.transfer_ring_.Restore(transaction);
    request.emplace(std::move(*pending_transfer.context->request));
  });
  status = ep.ControlRequestAllocationPhase(&pending_transfer);
  if (status != ZX_OK) {
    return status;
  }
  status = ep.ControlRequestStatusPhase(&pending_transfer);
  if (status != ZX_OK) {
    return status;
  }
  status = ep.ControlRequestDataPhase(&pending_transfer);
  if (status != ZX_OK) {
    return status;
  }
  ep.ControlRequestSetupPhase(&pending_transfer);
  rollback.cancel();
  ep.ControlRequestCommit(&pending_transfer);
  return ZX_OK;
}

zx_status_t Endpoint::ControlRequestAllocationPhase(UsbRequestState* state) {
  state->setup_cycle = state->setup->status;
  state->setup->status = 0;
  size_t length = std::holds_alternative<usb::FidlRequest>(*state->context->request)
                      ? std::get<usb::FidlRequest>(*state->context->request).length()
                      : std::get<Request>(*state->context->request).request()->header.length;
  if (length) {
    auto status = std::visit([&](auto&& req) -> zx_status_t { return req.PhysMap(hci_->bti()); },
                             *state->context->request);
    if (status != ZX_OK) {
      return status;
    }

    auto iters = get_iter(*state->context->request, k64KiB);
    if (iters.is_error()) {
      return iters.error_value();
    }

    TRB* current_trb = nullptr;
    for (auto& iter : iters.value()) {
      for (auto [paddr, len] : iter) {
        if (!len) {
          break;
        }
        state->packet_count++;
        TRB* prev = current_trb;
        zx_status_t status = transfer_ring_.AllocateTRB(&current_trb, nullptr);
        if (status != ZX_OK) {
          return status;
        }
        static_assert(sizeof(TRB*) == sizeof(uint64_t));
        if (likely(prev)) {
          prev->ptr = reinterpret_cast<uint64_t>(current_trb);
        } else {
          state->first_trb = current_trb;
        }
      }
    }
  }
  return ZX_OK;
}

zx_status_t Endpoint::ControlRequestStatusPhase(UsbRequestState* state) {
  state->interrupter = 0;
  bool status_in = true;
  // See table 4-7 in section 4.11.2.2
  auto bm_request_type =
      std::holds_alternative<Request>(*state->context->request)
          ? std::get<Request>(*state->context->request).request()->setup.bm_request_type
          : std::get<usb::FidlRequest>(*state->context->request)
                ->information()
                ->control()
                ->setup()
                ->bm_request_type();
  if (state->first_trb && usb_request_is_in(bm_request_type)) {
    status_in = false;
  }
  zx_status_t status = transfer_ring_.AllocateTRB(&state->status_trb_ptr, nullptr);
  if (status != ZX_OK) {
    return status;
  }
  Control::FromTRB(state->status_trb_ptr)
      .set_Cycle(state->status_trb_ptr->status)
      .set_Type(Control::Status)
      .ToTrb(state->status_trb_ptr);
  state->status_trb_ptr->status = 0;
  auto* status_trb = static_cast<Status*>(state->status_trb_ptr);
  status_trb->set_DIRECTION(status_in).set_INTERRUPTER(state->interrupter).set_IOC(1);
  return ZX_OK;
}

zx_status_t Endpoint::ControlRequestDataPhase(UsbRequestState* state) {
  // Data stage
  if (state->first_trb) {
    auto iters = get_iter(*state->context->request, k64KiB);
    if (iters.is_error()) {
      return iters.error_value();
    }

    TRB* current = state->first_trb;
    for (auto& iter : iters.value()) {
      for (auto [paddr, len] : iter) {
        if (!len) {
          break;
        }
        state->packet_count--;
        TRB* next = reinterpret_cast<TRB*>(current->ptr);
        uint32_t pcs = current->status;
        current->status = 0;
        enum Control::Type type;
        if (current == state->first_trb) {
          type = Control::Data;
          ControlData* data = reinterpret_cast<ControlData*>(current);
          auto bm_request_type =
              std::holds_alternative<Request>(*state->context->request)
                  ? std::get<Request>(*state->context->request).request()->setup.bm_request_type
                  : std::get<usb::FidlRequest>(*state->context->request)
                        ->information()
                        ->control()
                        ->setup()
                        ->bm_request_type();
          // Control transfers always get interrupter 0 (we consider those to be low-priority)
          // TODO(https://fxbug.dev/42109334): Change bus snooping options based on input from
          // higher-level drivers.
          data->set_CHAIN(next != nullptr)
              .set_DIRECTION(usb_request_is_in(bm_request_type))
              .set_INTERRUPTER(0)
              .set_LENGTH(len)
              .set_SIZE(state->packet_count)
              .set_ISP(true)
              .set_NO_SNOOP(!hci_->HasCoherentCache());
        } else {
          type = Control::Normal;
          Normal* data = reinterpret_cast<Normal*>(current);
          data->set_CHAIN(next != nullptr)
              .set_INTERRUPTER(0)
              .set_LENGTH(len)
              .set_SIZE(state->packet_count)
              .set_ISP(true)
              .set_NO_SNOOP(!hci_->HasCoherentCache());
        }
        current->ptr = paddr;
        Control::FromTRB(current).set_Cycle(pcs).set_Type(type).ToTrb(current);
        current = next;
      }
    }
  }
  return ZX_OK;
}

void Endpoint::ControlRequestSetupPhase(UsbRequestState* state) {
  // Setup phase (4.11.2.2)
  auto bm_request_type =
      std::holds_alternative<Request>(*state->context->request)
          ? std::get<Request>(*state->context->request).request()->setup.bm_request_type
          : std::get<usb::FidlRequest>(*state->context->request)
                ->information()
                ->control()
                ->setup()
                ->bm_request_type();
  const auto& setup = std::holds_alternative<Request>(*state->context->request)
                          ? std::get<Request>(*state->context->request).request()->setup
                          : ToBanjo(*std::get<usb::FidlRequest>(*state->context->request)
                                         ->information()
                                         ->control()
                                         ->setup());
  memcpy(&state->setup->ptr, &setup, sizeof(setup));
  Setup* setup_trb = reinterpret_cast<Setup*>(state->setup);
  setup_trb->set_INTERRUPTER(state->interrupter)
      .set_length(8)
      .set_IDT(1)
      .set_TRT(usb_request_is_in(bm_request_type) ? Setup::IN : Setup::OUT);
  hw_mb();
}

void Endpoint::ControlRequestCommit(UsbRequestState* state) {
  // Start the transaction!
  if (!hci_->HasCoherentCache()) {
    if (std::holds_alternative<Request>(*state->context->request)) {
      usb_request_cache_flush_invalidate(
          std::get<Request>(*state->context->request).request(), 0,
          std::get<Request>(*state->context->request).request()->header.length);
    } else {
      auto& fidl_req = std::get<usb::FidlRequest>(*state->context->request);
      if (fidl_req.length() > 0) {
        zx_status_t status =
            fidl_req.CacheFlushInvalidate([this](const auto& buffer) { return GetMapped(buffer); });
        if (status != ZX_OK) {
          fdf::warn("ControlRequestCommit CacheFlushInvalidate failed: {}",
                    zx_status_get_string(status));
        }
      }
    }
  }
  transfer_ring_.AssignContext(state->status_trb_ptr, std::move(state->context), state->first_trb,
                               state->setup);
  Control::FromTRB(state->setup)
      .set_Type(Control::Setup)
      .set_Cycle(state->setup_cycle)
      .ToTrb(state->setup);
  transfer_ring_.CommitTransaction(state->transaction);
}

zx_status_t Endpoint::LockedQueue::NormalRequestQueueLocked(
    Endpoint& ep, DeviceState& state, std::optional<usb::RequestVariant>& request) {
  UsbRequestState pending_transfer;
  uint8_t index = static_cast<uint8_t>(XhciEndpointIndex(ep.ep_addr()) - 1);
  if (state.IsDisconnecting() || !state.GetSlot()) {
    return ZX_ERR_IO_NOT_PRESENT;
  }
  if (ep.transfer_ring_.stalled()) {
    return ZX_ERR_IO_REFUSED;
  }
  if (!state.GetInputContext()) {
    return ZX_ERR_IO_NOT_PRESENT;
  }
  auto* control = reinterpret_cast<uint32_t*>(state.GetInputContext()->virt());
  auto* endpoint_context = reinterpret_cast<EndpointContext*>(
      reinterpret_cast<unsigned char*>(control) + (ep.hci_->slot_size_bytes() * (2 + (index + 1))));
  if (!ep.transfer_ring_.active()) {
    return ZX_ERR_INTERNAL;
  }
  pending_transfer.burst_size = endpoint_context->MaxBurstSize() + 1;
  pending_transfer.max_packet_size = endpoint_context->MAX_PACKET_SIZE();
  pending_transfer.context = ep.transfer_ring_.AllocateContext();
  if (!pending_transfer.context) {
    return ZX_ERR_NO_MEMORY;
  }
  pending_transfer.context->request.emplace(std::move(*request));
  request.reset();
  auto restore_request =
      fit::defer([&] { request.emplace(std::move(*pending_transfer.context->request)); });

  if (ep.transfer_ring_.IsIsochronous()) {
    // Isoc endpoints of the default interface (i.e. b_alternate_setting=0) are required to have a
    // w_max_packet_size=0 to prevent enumerated devices from reserving bandwidth by default. We'll
    // consider any request for a zero-length pipe as invalid (see USB 2.0 spec. 5.6.2).
    if (pending_transfer.max_packet_size == 0) {
      return ZX_ERR_INVALID_ARGS;
    }

    // Release the lock while we're sleeping to avoid blocking
    // other operations.
    uint64_t target_frame =
        std::holds_alternative<usb::FidlRequest>(*pending_transfer.context->request)
            ? *std::get<usb::FidlRequest>(*pending_transfer.context->request)
                   ->information()
                   ->isochronous()
                   ->frame_id()
            : std::get<Request>(*pending_transfer.context->request).request()->header.frame;
    state.transaction_lock().Release();
    auto status = ep.WaitForIsochronousReady(target_frame);
    state.transaction_lock().Acquire();
    if (status != ZX_OK) {
      return status;
    }

    // Check again since we've re-acquired lock
    if (!ep.hci_->Running()) {
      return ZX_ERR_BAD_STATE;
    }
    if (state.IsDisconnecting() || !state.GetSlot()) {
      return ZX_ERR_IO_NOT_PRESENT;
    }
    if (ep.transfer_ring_.stalled()) {
      return ZX_ERR_IO_REFUSED;
    }
    if (!ep.transfer_ring_.active()) {
      return ZX_ERR_INTERNAL;
    }
  }

  // Start the transaction
  pending_transfer.transaction = ep.transfer_ring_.SaveState();
  auto status = ep.StartNormalTransaction(&pending_transfer,
                                          static_cast<uint8_t>(state.GetInterrupterTarget()));
  if (status != ZX_OK) {
    ep.transfer_ring_.Restore(pending_transfer.transaction);
    return status;
  }
  // Continue the transaction
  status = ep.ContinueNormalTransaction(&pending_transfer);
  if (status != ZX_OK) {
    ep.transfer_ring_.Restore(pending_transfer.transaction);
    return status;
  }
  // Commit the transaction -- starting the actual transfer
  restore_request.cancel();
  ep.CommitNormalTransaction(&pending_transfer);
  return ZX_OK;
}

zx_status_t Endpoint::WaitForIsochronousReady(uint64_t target_frame) {
  // Cannot schedule more than 895 ms into the future per section 4.11.2.5
  // in the xHCI specification (revision 1.2)
  constexpr int kMaxSchedulingInterval = 895;
  if (target_frame) {
    if (!hci_->Running()) {
      return ZX_ERR_BAD_STATE;
    }
    uint64_t frame = hci_->UsbHciGetCurrentFrame();
    while (static_cast<int32_t>(target_frame - frame) > kMaxSchedulingInterval) {
      uint32_t time = static_cast<uint32_t>((target_frame - frame) - kMaxSchedulingInterval);
      zx::nanosleep(zx::deadline_after(zx::msec(time)));
      if (!hci_->Running()) {
        return ZX_ERR_BAD_STATE;
      }
      frame = hci_->UsbHciGetCurrentFrame();
    }

    if (target_frame < frame) {
      return ZX_ERR_IO_MISSED_DEADLINE;
    }
  }
  return ZX_OK;
}

zx_status_t Endpoint::StartNormalTransaction(UsbRequestState* state, uint8_t interrupter_target) {
  size_t packet_count = 0;

  // Normal transfer
  auto status = std::visit([&](auto&& req) -> zx_status_t { return req.PhysMap(hci_->bti()); },
                           *state->context->request);
  if (status != ZX_OK) {
    return status;
  }
  size_t pending_len = std::holds_alternative<usb::FidlRequest>(*state->context->request)
                           ? std::get<usb::FidlRequest>(*state->context->request).length()
                           : std::get<Request>(*state->context->request).request()->header.length;
  uint32_t total_len = 0;
  auto iters = get_iter(*state->context->request, k64KiB);
  if (iters.is_error()) {
    return iters.error_value();
  }

  for (auto& iter : iters.value()) {
    for (auto [paddr, len] : iter) {
      if (len > pending_len) {
        len = pending_len;
      }
      if (!paddr) {
        break;
      }
      if (!len) {
        continue;
      }
      total_len += static_cast<uint32_t>(len);
      packet_count++;
      pending_len -= len;
    }
  }

  if (pending_len) {
    // Something doesn't add up here....
    return ZX_ERR_BAD_STATE;
  }
  // Allocate contiguous memory
  auto contig_trb_info = transfer_ring_.AllocateContiguous(packet_count);
  if (contig_trb_info.is_error()) {
    return contig_trb_info.error_value();
  }
  state->info = contig_trb_info.value();
  state->total_len = total_len;
  state->packet_count = packet_count;
  state->first_cycle = state->info.first()[0].status;
  state->first_trb = state->info.first().data();
  state->last_trb = state->info.trbs.data() + (packet_count - 1);
  state->interrupter = static_cast<uint8_t>(interrupter_target);

  return ZX_OK;
}

zx_status_t Endpoint::ContinueNormalTransaction(UsbRequestState* state) {
  // Data stage
  size_t pending_len = std::holds_alternative<usb::FidlRequest>(*state->context->request)
                           ? std::get<usb::FidlRequest>(*state->context->request).length()
                           : std::get<Request>(*state->context->request).request()->header.length;
  for (TRB& nop_trb : state->info.nop) {
    bool producer_cycle_state = nop_trb.status;
    bool cycle = (&nop_trb == state->first_trb) ? !producer_cycle_state : producer_cycle_state;
    Control::FromTRB(&nop_trb).set_Cycle(cycle).ToTrb(&nop_trb);
    nop_trb.status = 0;
  }
  if (state->first_trb) {
    TRB* current = state->info.trbs.data();
    auto iters = get_iter(*state->context->request, k64KiB);
    if (iters.is_error()) {
      return iters.error_value();
    }

    for (auto& iter : iters.value()) {
      for (auto [paddr, len] : iter) {
        if (!len) {
          break;
        }
        len = std::min(len, pending_len);
        pending_len -= len;
        state->packet_count--;
        TRB* next = current + 1;
        if (next == state->last_trb + 1) {
          next = nullptr;
        }
        uint32_t pcs = current->status;
        current->status = 0;
        enum Control::Type type;
        if (transfer_ring_.IsIsochronous() && (current == state->info.trbs.data())) {
          // Force direct mode as workaround for USB audio latency issue.
          type = Control::Isoch;
          Isoch* data = reinterpret_cast<Isoch*>(current);
          // Burst size is number of packets, not bytes
          uint32_t burst_size = state->burst_size;
          uint32_t packet_size = state->max_packet_size;
          uint32_t packet_count = state->total_len / packet_size;
          if (!packet_count) {
            packet_count = 1;
          }
          // Number of bursts - 1
          uint32_t burst_count = packet_count / burst_size;
          if (burst_count) {
            burst_count--;
          }
          // Zero-based last-burst-packet count (where 0 == 1 packet)
          uint32_t last_burst_packet_count = packet_count % burst_size;
          if (last_burst_packet_count) {
            last_burst_packet_count--;
          }
          auto frame_id = std::holds_alternative<usb::FidlRequest>(*state->context->request)
                              ? *std::get<usb::FidlRequest>(*state->context->request)
                                     ->information()
                                     ->isochronous()
                                     ->frame_id()
                              : std::get<Request>(*state->context->request).request()->header.frame;
          data->set_CHAIN(next != nullptr)
              .set_SIA(frame_id == 0)
              .set_TLBPC(last_burst_packet_count)
              .set_FrameID(frame_id % 2048)
              .set_TBC(burst_count)
              .set_INTERRUPTER(state->interrupter)
              .set_LENGTH(len)
              .set_SIZE(packet_count)
              .set_NO_SNOOP(!hci_->HasCoherentCache())
              .set_IOC(next == nullptr)
              .set_ISP(true);
        } else {
          type = Control::Normal;
          Normal* data = reinterpret_cast<Normal*>(current);
          data->set_CHAIN(next != nullptr)
              .set_INTERRUPTER(state->interrupter)
              .set_LENGTH(len)
              .set_SIZE(state->packet_count)
              .set_NO_SNOOP(!hci_->HasCoherentCache())
              .set_IOC(next == nullptr)
              .set_ISP(true);
        }

        current->ptr = paddr;
        Control::FromTRB(current)
            .set_Cycle(unlikely(current == state->first_trb) ? !pcs : pcs)
            .set_Type(type)
            .ToTrb(current);
        current = next;
      }
    }
  }
  return ZX_OK;
}

void Endpoint::CommitNormalTransaction(UsbRequestState* state) {
  hw_mb();
  // Start the transaction!
  if (!hci_->HasCoherentCache()) {
    if (std::holds_alternative<Request>(*state->context->request)) {
      usb_request_cache_flush_invalidate(
          std::get<Request>(*state->context->request).request(), 0,
          std::get<Request>(*state->context->request).request()->header.length);
    } else {
      auto& fidl_req = std::get<usb::FidlRequest>(*state->context->request);
      if (fidl_req.length() > 0) {
        zx_status_t status =
            fidl_req.CacheFlushInvalidate([this](const auto& buffer) { return GetMapped(buffer); });
        if (status != ZX_OK) {
          fdf::warn("CommitNormalTransaction CacheFlushInvalidate failed: {}",
                    zx_status_get_string(status));
        }
      }
    }
  }
  transfer_ring_.AssignContext(state->last_trb, std::move(state->context), state->first_trb);
  if (!hci_->HasCoherentState()) {
    if (!state->info.nop.empty()) {
      zx_cache_flush(state->info.nop.data(), state->info.nop.size_bytes(), ZX_CACHE_FLUSH_DATA);
    }
    zx_cache_flush(state->info.trbs.data(), state->info.trbs.size_bytes(), ZX_CACHE_FLUSH_DATA);
  }
  hw_mb();
  Control::FromTRB(state->first_trb).set_Cycle(state->first_cycle).ToTrb(state->first_trb);

  transfer_ring_.CommitTransaction(state->transaction);
}

}  // namespace usb_xhci
