// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/ethernet/drivers/usb-cdc-ecm/usb-cdc-ecm.h"

#include <endian.h>
#include <fuchsia/hardware/usb/c/banjo.h>
#include <fuchsia/hardware/usb/composite/c/banjo.h>
#include <fuchsia/hardware/usb/composite/cpp/banjo.h>
#include <lib/driver/compat/cpp/banjo_client.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/fit/defer.h>
#include <lib/operation/ethernet.h>
#include <sys/types.h>
#include <zircon/errors.h>
#include <zircon/status.h>

#include <cinttypes>
#include <tuple>
#include <utility>
#include <vector>

#include <usb/cdc.h>
#include <usb/descriptors.h>
#include <usb/request-cpp.h>
#include <usb/usb-request.h>

#include "src/connectivity/ethernet/drivers/usb-cdc-ecm/usb-cdc-ecm-lib.h"

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace {

// The maximum amount of memory we are willing to allocate to transaction buffers
constexpr size_t kMaxTxBufferSize = 32768;
constexpr size_t kMaxRxBufferSize = (1500 * 2048);
constexpr uint64_t kEthernetMaxTransmitDelay = 100;
constexpr uint64_t kEthernetMaxRecvDelay = 100;
constexpr uint64_t kEthernetTransmitDelay = 10;
constexpr uint64_t kEthernetRecvDelay = 10;
constexpr uint64_t kEthernetInitialTransmitDelay = 0;
constexpr uint64_t kEthernetInitialRecvDelay = 0;
constexpr uint16_t kEthernetInitialPacketFilter =
    (fdescriptor::kCdcPacketTypeDirected | fdescriptor::kCdcPacketTypeBroadcast |
     fdescriptor::kCdcPacketTypeMulticast);

}  // namespace

namespace usb_cdc_ecm {

static bool WantInterface(usb_interface_descriptor_t* intf, void* arg) {
  return intf->b_interface_class == fidl::ToUnderlying(fdescriptor::UsbClass::kCdc);
}

static bool HasCdcDataInterface(usb::UsbDevice& usb) {
  std::optional<usb::InterfaceList> interfaces;
  if (usb::InterfaceList::Create(usb, /*skip_alt=*/true, &interfaces) != ZX_OK) {
    return false;
  }
  for (const usb::Interface& interface : *interfaces) {
    if (interface.descriptor()->b_interface_class ==
        fidl::ToUnderlying(fdescriptor::UsbClass::kCdc)) {
      return true;
    }
  }
  return false;
}

void UsbCdcEcm::Stop(fdf::StopCompleter completer) {
  int_wait_.Cancel();

  {
    fbl::AutoLock tx_lock(&mutex_);
    unbound_ = true;
    while (rx_requeues_in_progress_ > 0) {
      rx_requeues_done_.Wait(&mutex_);
    }
    // EthernetImplStop() returns the client's netbufs. Anything still queued belongs to a client
    // that is already gone, so release it without calling its completion callback.
    while (auto op = pending_tx_queue_.pop()) {
      std::ignore = op->take();
    }
  }

  // Cancel all pending USB transfers outside mutex_ so completions can acquire mutex_.
  auto cancel_all = [this](const std::optional<EcmEndpoint>& endpoint) {
    if (!endpoint.has_value()) {
      return;
    }
    if (zx_status_t status = usb_.CancelAll(endpoint->addr); status != ZX_OK) {
      fdf::warn("Failed to cancel transfers on endpoint 0x{:02x}: {}", endpoint->addr,
                zx_status_get_string(status));
    }
  };
  cancel_all(int_endpoint_);
  cancel_all(rx_endpoint_);
  cancel_all(tx_endpoint_);

  {
    fbl::AutoLock lock(&mutex_);
    if (in_flight_requests_ > 0) {
      stop_completer_.emplace(std::move(completer));
      return;
    }
  }
  completer(zx::ok());
}

std::optional<fdf::StopCompleter> UsbCdcEcm::TakeStopCompleterOnLastRequestLocked() {
  ZX_DEBUG_ASSERT(in_flight_requests_ > 0);
  if (--in_flight_requests_ == 0 && stop_completer_.has_value()) {
    return std::exchange(stop_completer_, std::nullopt);
  }
  return std::nullopt;
}

void UsbCdcEcm::UpdateOnlineStatus(bool is_online) {
  fbl::AutoLock lock(&mutex_);
  if (unbound_) {
    return;
  }
  fbl::AutoLock ethernet_lock(&ethernet_mutex_);
  if ((is_online && online_) || (!is_online && !online_)) {
    return;
  }

  usb_request_complete_callback_t callback = {
      .callback =
          [](void* ctx, usb_request_t* request) {
            static_cast<UsbCdcEcm*>(ctx)->UsbReadComplete(request);
          },
      .ctx = this,
  };

  if (is_online) {
    fdf::info("Connected to network");
    online_ = true;

    std::optional<usb::Request<>> request;
    size_t request_size = usb::Request<>::RequestSize(parent_req_size_);
    while ((request = rx_request_pool_.Get(request_size))) {
      in_flight_requests_++;
      usb_.RequestQueue(request->take(), &callback);
    }

    if (ethernet_ifc_.ops) {
      ethernet_ifc_status(&ethernet_ifc_, ETHERNET_STATUS_ONLINE);
    } else {
      fdf::warn("Not connected to ethermac interface");
    }
  } else {
    fdf::info("No connection to network");
    online_ = false;
    if (ethernet_ifc_.ops) {
      ethernet_ifc_status(&ethernet_ifc_, 0);
    }
  }
}

zx_status_t UsbCdcEcm::EthernetImplQuery(uint32_t options, ethernet_info_t* info) {
  fdf::debug("{} called", __FUNCTION__);

  // No options are supported
  if (options) {
    fdf::error("Unexpected options (0x{:08x}) to EthernetImplQuery", options);
    return ZX_ERR_INVALID_ARGS;
  }

  *info = {};
  info->mtu = mtu_;
  memcpy(info->mac, mac_addr_.data(), mac_addr_.size());
  info->netbuf_size = eth::BorrowedOperation<>::OperationSize(sizeof(ethernet_netbuf_t));

  return ZX_OK;
}

void UsbCdcEcm::EthernetImplStop() {
  eth::BorrowedOperationQueue<> pending_tx;
  {
    fbl::AutoLock tx_lock(&mutex_);
    {
      fbl::AutoLock ethernet_lock(&ethernet_mutex_);
      ethernet_ifc_.ops = nullptr;
    }
    pending_tx = std::move(pending_tx_queue_);
    while (tx_completions_in_progress_ > 0) {
      tx_completions_done_.Wait(&mutex_);
    }
  }
  // Return queued netbufs while the client that owns their completion callbacks is alive.
  pending_tx.CompleteAll(ZX_ERR_UNAVAILABLE);
}

zx_status_t UsbCdcEcm::EthernetImplStart(const ethernet_ifc_protocol_t* ifc) {
  fbl::AutoLock ethernet_lock(&ethernet_mutex_);
  zx_status_t status = ZX_OK;

  if (ethernet_ifc_.ops != nullptr) {
    status = ZX_ERR_ALREADY_BOUND;
  } else {
    ethernet_ifc_ = *ifc;
    ethernet_ifc_status(&ethernet_ifc_, online_ ? ETHERNET_STATUS_ONLINE : 0);
  }

  return status;
}

void UsbCdcEcm::EthernetImplQueueTx(uint32_t options, ethernet_netbuf_t* netbuf,
                                    ethernet_impl_queue_tx_callback completion_cb, void* cookie) {
  fdf::trace("{} called", __FUNCTION__);
  eth::BorrowedOperation<> op(netbuf, completion_cb, cookie, sizeof(ethernet_netbuf_t));

  size_t length = op.operation()->data_size;
  if (length > mtu_ || length == 0) {
    op.Complete(ZX_ERR_INVALID_ARGS);
    return;
  }

  fdf::trace("Sending {} bytes to endpoint 0x{:08x}", length, tx_endpoint_->addr);

  fbl::AutoLock lock(&mutex_);

  if (unbound_) {
    lock.release();
    op.Complete(ZX_ERR_IO_NOT_PRESENT);
    return;
  }
  {
    fbl::AutoLock ethernet_lock(&ethernet_mutex_);
    if (ethernet_ifc_.ops == nullptr) {
      ethernet_lock.release();
      lock.release();
      op.Complete(ZX_ERR_BAD_STATE);
      return;
    }
  }

  if (!pending_tx_queue_.is_empty()) {
    pending_tx_queue_.push(std::move(op));
    return;
  }

  zx_status_t status = SendLocked(op);
  if (status == ZX_ERR_SHOULD_WAIT) {
    pending_tx_queue_.push(std::move(op));
  } else {
    lock.release();
    op.Complete(status);
  }
}

zx_status_t UsbCdcEcm::EthernetImplSetParam(uint32_t param, int32_t value, const uint8_t* data,
                                            size_t data_size) {
  fdf::debug("{} called", __FUNCTION__);
  zx_status_t status;

  switch (param) {
    case ETHERNET_SETPARAM_PROMISC:
      status =
          SetPacketFilterMode(fdescriptor::kCdcPacketTypePromiscuous, static_cast<bool>(value));
      break;
    default:
      status = ZX_ERR_NOT_SUPPORTED;
  }

  return status;
}

zx_status_t UsbCdcEcm::SetPacketFilterMode(uint16_t mode, bool on) {
  zx_status_t status = ZX_OK;
  uint16_t bits = rx_packet_filter_;

  if (on) {
    bits |= mode;
  } else {
    bits &= ~mode;
  }

  status = usb_.ControlOut(kClassInterfaceOut,
                           fidl::ToUnderlying(fdescriptor::CdcRequest::kSetEthernetPacketFilter),
                           bits, comm_intf_num_, ZX_TIME_INFINITE, nullptr, 0);

  if (status != ZX_OK) {
    fdf::error("Set packet filter failed: {}", status);
    return status;
  }
  rx_packet_filter_ = bits;
  return status;
}

void UsbCdcEcm::HandleInterrupt(usb::Request<void>& request) {
  if (request.request()->response.actual < sizeof(usb_cdc_notification_t)) {
    fdf::debug("Ignored interrupt (size = {})", request.request()->response.actual);
    return;
  }

  usb_cdc_notification_t usb_req = {};
  ssize_t result = request.CopyFrom(&usb_req, sizeof(usb_cdc_notification_t), 0);
  if (result != static_cast<ssize_t>(sizeof(usb_cdc_notification_t))) {
    fdf::debug("Ignored interrupt (copied {} from request)", result);
    return;
  }

  if (usb_req.bmRequestType == kClassInterfaceIn &&
      usb_req.bNotification == fdescriptor::CdcNotification::kNetworkConnection) {
    UpdateOnlineStatus(usb_req.wValue != 0);
  } else if (usb_req.bmRequestType == kClassInterfaceIn &&
             usb_req.bNotification == fdescriptor::CdcNotification::kConnectionSpeedChange) {
    // Per USB Class Definitions for Communications Devices (CDC) v1.2, Section 6.3.2
    // ("ConnectionSpeedChange", Table 69 & Table 70), wLength is 8 and the 8-byte
    // ConnectionSpeedChange data structure carries two 32-bit little-endian bit rates
    // (DLBitRate followed by ULBitRate). The ethermac driver doesn't care about speed changes, so
    // even though we track this information, it's currently unused.
    const uint16_t notif_length = letoh16(usb_req.wLength);
    if (notif_length != 8 ||
        request.request()->response.actual < sizeof(usb_cdc_speed_change_notification_t)) {
      fdf::error("Invalid size (wLength={}, actual={}) for CONNECTION_SPEED_CHANGE notification",
                 notif_length, request.request()->response.actual);
      return;
    }

    // Data immediately follows notification in packet: the downlink rate, then the uplink rate.
    usb_cdc_speed_change_notification_t speed_change = {};
    result = request.CopyFrom(&speed_change, sizeof(speed_change), 0);
    if (result != static_cast<ssize_t>(sizeof(speed_change))) {
      fdf::error("Failed to read connection speeds: {}", result);
      return;
    }
    const uint32_t new_ds_bps = letoh32(speed_change.downlink_br);
    const uint32_t new_us_bps = letoh32(speed_change.uplink_br);

    if (new_us_bps != us_bps_.load(std::memory_order_relaxed)) {
      fdf::info("Connection speed change... upstream bits/s: {}", new_us_bps);
      us_bps_.store(new_us_bps, std::memory_order_relaxed);
    }
    if (new_ds_bps != ds_bps_.load(std::memory_order_relaxed)) {
      fdf::info("Connection speed change... downstream bits/s: {}", new_ds_bps);
      ds_bps_.store(new_ds_bps, std::memory_order_relaxed);
    }
  } else {
    fdf::error("Ignored interrupt (type = {}, request = {})", usb_req.bmRequestType,
               usb_req.bNotification);
    return;
  }
}

void UsbCdcEcm::InterruptComplete(usb_request_t* request) {
  int_event_.signal(0, ZX_USER_SIGNAL_0);

  std::optional<fdf::StopCompleter> completer;
  {
    fbl::AutoLock lock(&mutex_);
    if (auto c = TakeStopCompleterOnLastRequestLocked()) {
      completer.emplace(std::move(*c));
    }
  }
  if (completer.has_value()) {
    (*completer)(zx::ok());
  }
}

void UsbCdcEcm::ScheduleInterruptTask(async_dispatcher_t* dispatcher) {
  usb_request_complete_callback_t complete = {
      .callback = [](void* ctx, usb_request_t* request) -> void {
        static_cast<UsbCdcEcm*>(ctx)->InterruptComplete(request);
      },
      .ctx = this,
  };

  int_event_.signal(ZX_USER_SIGNAL_0, 0);  // clear signal.
  usb_request_t* req = nullptr;
  {
    fbl::AutoLock lock(&mutex_);
    if (unbound_) {
      return;
    }
    in_flight_requests_++;
    req = interrupt_request_->request();
    int_wait_.Begin(dispatcher);
  }
  usb_.RequestQueue(req, &complete);
}

void UsbCdcEcm::TaskHandler(async_dispatcher_t* dispatcher, async::WaitBase* wait,
                            zx_status_t status, const zx_packet_signal_t* signal) {
  if (status == ZX_ERR_CANCELED) {
    return;
  }

  zx_status_t request_status = interrupt_request_->request()->response.status;
  if (request_status == ZX_OK) {
    HandleInterrupt(interrupt_request_.value());
  } else if (request_status == ZX_ERR_PEER_CLOSED || request_status == ZX_ERR_IO_NOT_PRESENT ||
             request_status == ZX_ERR_CANCELED) {
    fdf::debug("Terminating interrupt handling thread");
    return;
  } else if (request_status == ZX_ERR_IO_REFUSED || request_status == ZX_ERR_IO_INVALID) {
    fdf::debug("Resetting interrupt endpoint");
    usb_.ResetEndpoint(int_endpoint_->addr);
  } else {
    fdf::error("Error waiting for interrupt - ignoring: {}", zx_status_get_string(request_status));
  }

  ScheduleInterruptTask(dispatcher);
}

zx_status_t UsbCdcEcm::Init() {
  fdf::debug("Starting {}", __FUNCTION__);

  // Find the CDC descriptors and endpoints
  auto parser = UsbCdcDescriptorParser::Parse(usb_);
  if (parser.is_error()) {
    fdf::error("Failed to parse usb descriptor: {}", parser);
    return parser.error_value();
  }

  comm_intf_num_ = parser->GetCommInterface().number;

  // Initialize context
  zx_status_t status = usb_.ControlOut(
      kClassInterfaceOut, fidl::ToUnderlying(fdescriptor::CdcRequest::kSetEthernetPacketFilter),
      kEthernetInitialPacketFilter, comm_intf_num_, ZX_TIME_INFINITE, nullptr, 0);
  if (status != ZX_OK) {
    fdf::error("Failed to set initial packet filter: {}", zx_status_get_string(status));
    return status;
  }
  rx_packet_filter_ = kEthernetInitialPacketFilter;

  // Parse endpoint information
  int_endpoint_ = parser->GetInterruptEndpoint();
  tx_endpoint_ = parser->GetTxEndpoint();
  rx_endpoint_ = parser->GetRxEndpoint();

  EcmInterface default_ifc = parser->GetDefaultInterface();
  EcmInterface data_ifc = parser->GetDataInterface();

  mtu_ = parser->GetMtu();
  mac_addr_ = parser->GetMacAddress();

  rx_endpoint_delay_ = kEthernetInitialRecvDelay;
  tx_endpoint_delay_ = kEthernetInitialTransmitDelay;

  // Reset by selecting default interface followed by data interface. We can't start
  // queueing transactions until this is complete.
  usb_.SetInterface(default_ifc.number, default_ifc.alternate_setting);
  usb_.SetInterface(data_ifc.number, data_ifc.alternate_setting);

  // Allocate interrupt transaction buffer
  parent_req_size_ = usb_.GetRequestSize();
  status = usb::Request<void>::Alloc(&interrupt_request_, int_endpoint_->max_packet_size,
                                     int_endpoint_->addr, parent_req_size_);
  if (status != ZX_OK) {
    return status;
  }

  // Allocate tx transaction buffers
  uint16_t tx_buf_sz = mtu_;
  if (tx_buf_sz > kMaxTxBufferSize) {
    fdf::error("Insufficient space for even a single tx buffer");
    return status;
  }

  fbl::AutoLock lock(&mutex_);

  size_t tx_buf_remain = kMaxTxBufferSize;
  while (tx_buf_remain >= tx_buf_sz) {
    std::optional<usb::Request<void>> request;
    status = usb::Request<void>::Alloc(&request, tx_buf_sz, tx_endpoint_->addr, parent_req_size_);
    if (status != ZX_OK) {
      return status;
    }
    request->request()->direct = true;

    // As per the CDC-ECM spec, we need to send a zero-length packet to signify the end of
    // transmission when the endpoint max packet size is a factor of the total transmission size
    request->request()->header.send_zlp = true;
    tx_request_pool_.Add(*std::move(request));

    tx_buf_remain -= tx_buf_sz;
  }

  // Allocate rx transaction buffers
  uint32_t rx_buf_sz = mtu_;
  if (rx_buf_sz > kMaxRxBufferSize) {
    fdf::error("Insufficient space for even a single rx buffer");
    return ZX_ERR_NO_MEMORY;
  }

  size_t rx_buf_remain = kMaxRxBufferSize;
  while (rx_buf_remain >= rx_buf_sz) {
    std::optional<usb::Request<void>> request;
    status = usb::Request<void>::Alloc(&request, rx_buf_sz, rx_endpoint_->addr, parent_req_size_);
    if (status != ZX_OK) {
      return status;
    }

    request->request()->direct = true;
    rx_request_pool_.Add(*std::move(request));
    rx_buf_remain -= rx_buf_sz;
  }

  return ZX_OK;
}

zx::result<> UsbCdcEcm::Start(fdf::DriverContext context) {
  fdf::debug("Starting {}", __FUNCTION__);

  auto incoming = std::shared_ptr<fdf::Namespace>(context.take_incoming());

  zx::result<ddk::UsbProtocolClient> usb_client =
      compat::ConnectBanjo<ddk::UsbProtocolClient>(incoming);
  if (usb_client.is_error()) {
    fdf::error("Failed to connect to USB banjo protocol: {}", usb_client);
    return usb_client.take_error();
  }
  usb_protocol_t usb_proto;
  usb_client->GetProto(&usb_proto);
  usb_ = usb::UsbDevice(&usb_proto);
  if (!usb_.is_valid()) {
    fdf::error("Received invalid USB banjo protocol client");
    return zx::error(ZX_ERR_PROTOCOL_NOT_SUPPORTED);
  }

  // When bound to an Interface Association Descriptor (BIND_PROTOCOL.INTERFACE_ASSOCIATION),
  // usb-composite already includes both the communications and CDC data interfaces in usb_'s
  // descriptor list. Only claim additional interfaces when bound to the standalone communications
  // interface (BIND_PROTOCOL.INTERFACE).
  if (!HasCdcDataInterface(usb_)) {
    zx::result<ddk::UsbCompositeProtocolClient> usb_composite_client =
        compat::ConnectBanjo<ddk::UsbCompositeProtocolClient>(incoming);
    if (usb_composite_client.is_error()) {
      fdf::error("Failed to connect to USB composite banjo protocol: {}", usb_composite_client);
      return usb_composite_client.take_error();
    }
    usb_composite_protocol_t usb_composite;
    usb_composite_client->GetProto(&usb_composite);

    if (zx_status_t status =
            usb_claim_additional_interfaces(&usb_composite, WantInterface, nullptr);
        status != ZX_OK) {
      fdf::error("Failed to claim additional interfaces: {}", zx::make_result(status));
      return zx::error(status);
    }
  }

  zx_status_t status = zx::event::create(0, &int_event_);
  if (status != ZX_OK) {
    fdf::error("Failed to create interrupt event ({})", zx_status_get_string(status));
    return zx::error(status);
  }
  int_wait_.set_object(int_event_.get());
  int_wait_.set_trigger(ZX_USER_SIGNAL_0);

  if (zx_status_t status = Init(); status != ZX_OK) {
    fdf::error("Failed to initialize device: {}", zx::make_result(status));
    return zx::error(status);
  }

  // Serve the EthernetImpl banjo protocol to the child node through the compat device server.
  compat::DeviceServer::BanjoConfig banjo_config;
  banjo_config.callbacks[ZX_PROTOCOL_ETHERNET_IMPL] = banjo_server_.callback();

  device_server_.Initialize(std::string(kChildNodeName), std::nullopt, std::move(banjo_config));
  if (zx_status_t status = device_server_.Serve(dispatcher(), outgoing().get()); status != ZX_OK) {
    fdf::error("Failed to serve compat device server: {}", zx::make_result(status));
    return zx::error(status);
  }

  const std::array<fuchsia_driver_framework::NodeProperty2, 1> properties = {
      banjo_server_.property()};
  std::vector<fuchsia_driver_framework::Offer> offers = device_server_.CreateOffers2();

  zx::result child = AddChild(kChildNodeName, properties, offers);
  if (child.is_error()) {
    fdf::error("Error adding child node: {}", child);
    return child.take_error();
  }
  child_ = std::move(child.value());

  ScheduleInterruptTask(dispatcher());
  return zx::ok();
}

zx_status_t UsbCdcEcm::SendLocked(const eth::BorrowedOperation<void>& op) {
  // Make sure that we can get all of the tx buffers we need to use
  std::optional<usb::Request<>> request;
  request = tx_request_pool_.Get(usb::Request<>::RequestSize(parent_req_size_));
  if (!request.has_value()) {
    return ZX_ERR_SHOULD_WAIT;
  }

  zx_nanosleep(zx_deadline_after(ZX_USEC(tx_endpoint_delay_)));

  request->request()->header.length = op.operation()->data_size;
  ssize_t bytes_copied = request->CopyTo(op.operation()->data_buffer, op.operation()->data_size, 0);
  if (bytes_copied < 0) {
    fdf::error("Failed to copy data into send txn (error {})", bytes_copied);
    tx_request_pool_.Add(*std::move(request));
    return ZX_ERR_IO;
  }

  usb_request_complete_callback_t complete = {
      .callback =
          [](void* ctx, usb_request_t* request) {
            static_cast<UsbCdcEcm*>(ctx)->UsbWriteComplete(request);
          },
      .ctx = this,
  };
  in_flight_requests_++;
  usb_.RequestQueue(request->take(), &complete);
  return ZX_OK;
}

void UsbCdcEcm::UsbReadComplete(usb_request_t* usb_request) {
  usb::Request<> request(usb_request, parent_req_size_);
  const zx_status_t response_status = request.request()->response.status;

  if (response_status != ZX_OK) {
    fdf::debug("UsbReadComplete called with status {}", zx_status_get_string(response_status));
  }

  auto request_cleanup = fit::defer([this, &request, response_status]() {
    std::optional<fdf::StopCompleter> completer;
    bool requeue = false;
    {
      fbl::AutoLock lock(&mutex_);
      if (unbound_ || response_status == ZX_ERR_IO_NOT_PRESENT ||
          response_status == ZX_ERR_CANCELED || response_status == ZX_ERR_PEER_CLOSED) {
        rx_request_pool_.Add(std::move(request));
        if (auto c = TakeStopCompleterOnLastRequestLocked()) {
          completer.emplace(std::move(*c));
        }
      } else {
        // Stop() waits for in-progress requeues before it cancels the rx endpoint.
        rx_requeues_in_progress_++;
        requeue = true;
      }
    }
    if (requeue) {
      usb_request_complete_callback_t complete = {
          .callback =
              [](void* ctx, usb_request_t* request) {
                static_cast<UsbCdcEcm*>(ctx)->UsbReadComplete(request);
              },
          .ctx = this,
      };
      // Queued without mutex_ held: the HCI may complete the request inline, re-entering here.
      usb_.RequestQueue(request.take(), &complete);
      fbl::AutoLock lock(&mutex_);
      if (--rx_requeues_in_progress_ == 0) {
        rx_requeues_done_.Broadcast();
      }
    }
    if (completer.has_value()) {
      (*completer)(zx::ok());
    }
  });

  if (response_status == ZX_ERR_IO_NOT_PRESENT) {
    fdf::warn("USB device not present");
    return;
  }
  if (response_status == ZX_ERR_CANCELED || response_status == ZX_ERR_PEER_CLOSED) {
    return;
  }

  if (request.request()->response.status == ZX_ERR_IO_REFUSED) {
    fdf::debug("Resetting receive endpoint");
    usb_.ResetEndpoint(rx_endpoint_->addr);
    return;
  } else if (request.request()->response.status == ZX_ERR_IO_INVALID) {
    if (rx_endpoint_delay_ < kEthernetMaxRecvDelay) {
      rx_endpoint_delay_ += kEthernetMaxRecvDelay;
    }
    fdf::debug("Slowing down the requests by {} usec. Resetting the recv endpoint",
               kEthernetRecvDelay);
    usb_.ResetEndpoint(rx_endpoint_->addr);
    return;
  } else if (request.request()->response.status != ZX_OK) {
    fdf::warn("USB request status: {}", zx_status_get_string(request.request()->response.status));
    return;
  }

  void* read_data;
  const size_t len = request.request()->response.actual;
  const zx_status_t status = request.Mmap(&read_data);
  if (status != ZX_OK) {
    fdf::error("request.Mmap failed with status: {}", zx_status_get_string(status));
    return;
  }

  {
    fbl::AutoLock ethernet_lock(&ethernet_mutex_);
    if (ethernet_ifc_.ops) {
      ethernet_ifc_recv(&ethernet_ifc_, static_cast<uint8_t*>(read_data), len, 0);
    }
  }

  // Delay before requeueing the request.
  if (rx_endpoint_delay_) {
    zx_nanosleep(zx_deadline_after(ZX_USEC(rx_endpoint_delay_)));
  }
}

void UsbCdcEcm::UsbWriteComplete(usb_request_t* usb_request) {
  usb::Request<> request(usb_request, parent_req_size_);

  if (request.request()->response.status == ZX_ERR_IO_REFUSED) {
    fdf::debug("Resetting transmit endpoint");
    usb_.ResetEndpoint(tx_endpoint_->addr);

  } else if (request.request()->response.status == ZX_ERR_IO_INVALID) {
    fdf::debug("Slowing down the requests by {} usec. Resetting the transmit endpoint",
               kEthernetTransmitDelay);
    if (tx_endpoint_delay_ < kEthernetMaxTransmitDelay) {
      tx_endpoint_delay_ += kEthernetTransmitDelay;
    }
    usb_.ResetEndpoint(tx_endpoint_->addr);
  }

  // Return transmission buffer to pool
  std::optional<fdf::StopCompleter> completer;
  std::vector<std::pair<eth::BorrowedOperation<>, zx_status_t>> completed_ops;
  {
    fbl::AutoLock tx_lock(&mutex_);
    tx_request_pool_.Add(std::move(request));

    while (!unbound_ && !pending_tx_queue_.is_empty()) {
      auto op = pending_tx_queue_.pop().value();
      zx_status_t status = SendLocked(op);
      if (status == ZX_ERR_SHOULD_WAIT) {
        pending_tx_queue_.push_next(std::move(op));
        break;
      }
      completed_ops.emplace_back(std::move(op), status);
    }
    if (completed_ops.empty()) {
      if (auto c = TakeStopCompleterOnLastRequestLocked()) {
        completer.emplace(std::move(*c));
      }
    } else {
      tx_completions_in_progress_ += completed_ops.size();
    }
  }
  if (!completed_ops.empty()) {
    for (auto& [op, status] : completed_ops) {
      op.Complete(status);
    }
    fbl::AutoLock tx_lock(&mutex_);
    tx_completions_in_progress_ -= completed_ops.size();
    if (tx_completions_in_progress_ == 0) {
      tx_completions_done_.Broadcast();
    }
    if (auto c = TakeStopCompleterOnLastRequestLocked()) {
      completer.emplace(std::move(*c));
    }
  }
  if (completer.has_value()) {
    (*completer)(zx::ok());
  }
}

}  // namespace usb_cdc_ecm

FUCHSIA_DRIVER_EXPORT2(usb_cdc_ecm::UsbCdcEcm);
