// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/ethernet/drivers/usb-cdc-ecm/usb-cdc-ecm.h"

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

void UsbCdcEcm::Stop(fdf::StopCompleter completer) {
  int_wait_.Cancel();

  // TODO: Instead of taking the lock here and holding up the whole driver host, offload to the
  // worker thread and take ownership of the completer.
  {
    fbl::AutoLock tx_lock(&mutex_);
    unbound_ = true;
  }
  completer(zx::ok());
}

void UsbCdcEcm::UpdateOnlineStatus(bool is_online) {
  fbl::AutoLock lock(&mutex_);
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
  fbl::AutoLock tx_lock(&mutex_);
  fbl::AutoLock ethernet_lock(&ethernet_mutex_);
  ethernet_ifc_.ops = nullptr;
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

  if (!pending_tx_queue_.is_empty()) {
    pending_tx_queue_.push(std::move(op));
    return;
  }

  if (unbound_) {
    lock.release();
    op.Complete(ZX_ERR_IO_NOT_PRESENT);
  } else {
    zx_status_t status = SendLocked(op);
    if (status == ZX_ERR_SHOULD_WAIT) {
      pending_tx_queue_.push(std::move(op));
    } else {
      op.Complete(ZX_OK);
    }
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
    // The ethermac driver doesn't care about speed changes, so even though we track this
    // information, it's currently unused.
    if (usb_req.wLength != 8) {
      fdf::error("Invalid size ({}) for CONNECTION_SPEED_CHANGE notification", usb_req.wLength);
      return;
    }

    // Data immediately follows notification in packet
    uint32_t new_us_bps = 0, new_ds_bps = 0;
    result = request.CopyFrom(&new_us_bps, sizeof(new_us_bps), sizeof(usb_cdc_notification_t));
    if (result != static_cast<ssize_t>(sizeof(uint32_t))) {
      fdf::error("Failed to read new upstream speed: {}", result);
      return;
    }

    result = request.CopyFrom(&new_us_bps, sizeof(new_us_bps),
                              sizeof(usb_cdc_notification_t) + sizeof(uint32_t));
    if (result != static_cast<ssize_t>(sizeof(uint32_t))) {
      fdf::error("Failed to read new downstream speed: {}", result);
      return;
    }

    if (new_us_bps != us_bps_) {
      fdf::info("Connection speed change... upstream bits/s: {}", new_us_bps);
      us_bps_ = new_us_bps;
    }
    if (new_ds_bps != ds_bps_) {
      fdf::info("Connection speed change... downstream bits/s: {}", new_ds_bps);
      ds_bps_ = new_ds_bps;
    }
  } else {
    fdf::error("Ignored interrupt (type = {}, request = {})", usb_req.bmRequestType,
               usb_req.bNotification);
    return;
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
  usb_.RequestQueue(interrupt_request_->request(), &complete);
  int_wait_.Begin(dispatcher);
}

void UsbCdcEcm::TaskHandler(async_dispatcher_t* dispatcher, async::WaitBase* wait,
                            zx_status_t status, const zx_packet_signal_t* signal) {
  if (status == ZX_ERR_CANCELED) {
    return;
  }

  zx_status_t request_status = interrupt_request_->request()->response.status;
  if (request_status == ZX_OK) {
    HandleInterrupt(interrupt_request_.value());
  } else if (request_status == ZX_ERR_PEER_CLOSED || request_status == ZX_ERR_IO_NOT_PRESENT) {
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

  // Kick off the interrupt handler
  ScheduleInterruptTask(dispatcher());

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

  zx::result<ddk::UsbCompositeProtocolClient> usb_composite_client =
      compat::ConnectBanjo<ddk::UsbCompositeProtocolClient>(incoming);
  if (usb_composite_client.is_error()) {
    fdf::error("Failed to connect to USB composite banjo protocol: {}", usb_composite_client);
    return usb_composite_client.take_error();
  }
  usb_composite_protocol_t usb_composite;
  usb_composite_client->GetProto(&usb_composite);

  if (zx_status_t status = usb_claim_additional_interfaces(&usb_composite, WantInterface, nullptr);
      status != ZX_OK) {
    fdf::error("Failed to claim additional interfaces: {}", zx::make_result(status));
    return zx::error(status);
  }

  zx_status_t status = zx::event::create(0, &int_event_);
  if (status != ZX_OK) {
    fdf::error("Failed to create interrupt event ({})", zx_status_get_string(status));
    return zx::error(status);
  }

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

  {
    fbl::AutoLock ethernet_lock(&ethernet_mutex_);
    if (ethernet_ifc_.ops == nullptr) {
      fdf::error("No ethernet interface during QueueTx");
      tx_request_pool_.Add(*std::move(request));
      return ZX_ERR_BAD_STATE;
    }
  }

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
  usb_.RequestQueue(request->take(), &complete);
  return ZX_OK;
}

void UsbCdcEcm::UsbReadComplete(usb_request_t* usb_request) {
  usb::Request<> request(usb_request, parent_req_size_);

  if (request.request()->response.status != ZX_OK) {
    fdf::debug("UsbReadComplete called with status {}",
               zx_status_get_string(request.request()->response.status));
  }

  if (request.request()->response.status == ZX_ERR_IO_NOT_PRESENT) {
    fdf::warn("USB device not present");
    // The device has gone away, instead of requeueing the request - add it back to the pool. If the
    // device comes back online it will be queued then.
    fbl::AutoLock lock(&mutex_);
    rx_request_pool_.Add(std::move(request));
    return;
  }

  auto request_cleanup = fit::defer([this, &request]() {
    usb_request_complete_callback_t complete = {
        .callback =
            [](void* ctx, usb_request_t* request) {
              static_cast<UsbCdcEcm*>(ctx)->UsbReadComplete(request);
            },
        .ctx = this,
    };
    usb_.RequestQueue(request.take(), &complete);
  });

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
  fbl::AutoLock tx_lock(&mutex_);
  tx_request_pool_.Add(std::move(request));

  while (!pending_tx_queue_.is_empty()) {
    auto op = pending_tx_queue_.pop().value();
    zx_status_t status = SendLocked(op);
    if (status == ZX_ERR_SHOULD_WAIT) {
      pending_tx_queue_.push_next(std::move(op));
      break;
    }
    op.Complete(status);
  }
}

}  // namespace usb_cdc_ecm

FUCHSIA_DRIVER_EXPORT2(usb_cdc_ecm::UsbCdcEcm);
