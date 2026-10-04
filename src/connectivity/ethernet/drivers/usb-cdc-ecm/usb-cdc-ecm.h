// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file

#ifndef SRC_CONNECTIVITY_ETHERNET_DRIVERS_USB_CDC_ECM_USB_CDC_ECM_H_
#define SRC_CONNECTIVITY_ETHERNET_DRIVERS_USB_CDC_ECM_USB_CDC_ECM_H_

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fuchsia/hardware/ethernet/cpp/banjo.h>
#include <fuchsia/hardware/usb/c/banjo.h>
#include <fuchsia/hardware/usb/request/c/banjo.h>
#include <lib/async/cpp/wait.h>
#include <lib/driver/compat/cpp/banjo_server.h>
#include <lib/driver/compat/cpp/device_server.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/operation/ethernet.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <lib/zx/event.h>
#include <zircon/compiler.h>

#include <fbl/mutex.h>
#include <usb/usb.h>

#include "src/connectivity/ethernet/drivers/usb-cdc-ecm/usb-cdc-ecm-lib.h"
#include "usb/request-cpp.h"

namespace usb_cdc_ecm {

class UsbCdcEcm : public fdf::DriverBase2, public ddk::EthernetImplProtocol<UsbCdcEcm> {
 public:
  UsbCdcEcm() : fdf::DriverBase2("usb-cdc-ecm") {}
  ~UsbCdcEcm() override = default;

  // fdf::DriverBase2 implementation.
  zx::result<> Start(fdf::DriverContext context) override;
  void Stop(fdf::StopCompleter completer) override;

  // ZX_PROTOCOL_ETHERNET_IMPL ops.
  zx_status_t EthernetImplQuery(uint32_t options, ethernet_info_t* info);
  void EthernetImplStop();
  zx_status_t EthernetImplStart(const ethernet_ifc_protocol_t* ifc);
  void EthernetImplQueueTx(uint32_t options, ethernet_netbuf_t* netbuf,
                           ethernet_impl_queue_tx_callback completion_cb, void* cookie);
  zx_status_t EthernetImplSetParam(uint32_t param, int32_t value, const uint8_t* data,
                                   size_t data_size);
  void EthernetImplGetBti(zx::bti* bti) { bti->reset(); }

 private:
  // Name of the child node this driver publishes the EthernetImpl protocol on.
  static constexpr std::string_view kChildNodeName = "usb-cdc-ecm";

  // Parses the USB descriptors, allocates the transfer buffers and starts the interrupt handler
  // thread. Invoked from Start().
  zx_status_t Init();

  // The scheduled interrupt task re-schedules itself unless canceled or other terminal error.
  void ScheduleInterruptTask(async_dispatcher_t* dispatcher);
  void TaskHandler(async_dispatcher_t* dispatcher, async::WaitBase* wait, zx_status_t status,
                   const zx_packet_signal_t* signal);
  zx::event int_event_;
  async::WaitMethod<UsbCdcEcm, &UsbCdcEcm::TaskHandler> int_wait_{this};

  // Interrupt handler function invoked by the interrupt handler thread. It receives the usb_request
  // it has to work on. If the response is less than the size of (usb_cdc_notification_t) the
  // interrupt is ignored.
  void HandleInterrupt(usb::Request<void>& request);

  void InterruptComplete(usb_request_t* request) { int_event_.signal(0, ZX_USER_SIGNAL_0); }

  zx_status_t SetPacketFilterMode(uint16_t mode, bool on);

  // Returns with ZX_OK if its able to set the completion callback and queues the request
  // successfully. It returns with the appropriate error status otherwise.
  zx_status_t SendLocked(const eth::BorrowedOperation<void>& op) __TA_REQUIRES(&mutex_);

  void UsbReadComplete(usb_request_t* request);
  void UsbWriteComplete(usb_request_t* request);

  void UpdateOnlineStatus(bool is_online);

  usb::UsbDevice usb_;

  // Serves the fuchsia.driver.compat/Device protocol so that the child node can retrieve the
  // EthernetImpl banjo protocol below.
  compat::DeviceServer device_server_;
  compat::BanjoServer banjo_server_{ZX_PROTOCOL_ETHERNET_IMPL, this, &ethernet_impl_protocol_ops_};
  fidl::ClientEnd<fuchsia_driver_framework::NodeController> child_;

  // Ethernet lock -- must be acquired after tx_mutex_ when both locks are held.
  fbl::Mutex ethernet_mutex_;
  ethernet_ifc_protocol_t ethernet_ifc_ = {};

  // Device attributes
  MacAddress mac_addr_;
  uint16_t mtu_;

  // Connection attributes
  bool online_ __TA_GUARDED(ethernet_mutex_) = false;
  uint32_t ds_bps_ = 0;
  uint32_t us_bps_ = 0;

  // Interrupt handling
  std::optional<EcmEndpoint> int_endpoint_;
  std::optional<usb::Request<void>> interrupt_request_;
  sync_completion_t completion_;

  // Send context
  // TX lock -- Must be acquired before ethernet_mutex when both locks are held.
  fbl::Mutex mutex_;
  std::optional<EcmEndpoint> tx_endpoint_;
  usb::RequestPool<void> tx_request_pool_ TA_GUARDED(mutex_);
  eth::BorrowedOperationQueue<> pending_tx_queue_ TA_GUARDED(mutex_);
  bool unbound_ __TA_GUARDED(&mutex_) = false;
  uint64_t tx_endpoint_delay_;

  size_t parent_req_size_;

  // Receive context
  std::optional<EcmEndpoint> rx_endpoint_;
  usb::RequestPool<void> rx_request_pool_ TA_GUARDED(mutex_);
  uint64_t rx_endpoint_delay_;  // wait time between 2 recv requests
  uint16_t rx_packet_filter_;
  uint8_t comm_intf_num_ = 0;
};

}  // namespace usb_cdc_ecm

#endif  // SRC_CONNECTIVITY_ETHERNET_DRIVERS_USB_CDC_ECM_USB_CDC_ECM_H_
