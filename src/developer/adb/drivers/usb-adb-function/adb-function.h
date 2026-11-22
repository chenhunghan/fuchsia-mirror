// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_ADB_DRIVERS_USB_ADB_FUNCTION_ADB_FUNCTION_H_
#define SRC_DEVELOPER_ADB_DRIVERS_USB_ADB_FUNCTION_ADB_FUNCTION_H_

#include <endian.h>
#include <fidl/fuchsia.hardware.adb/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.descriptor/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.function/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async/cpp/task.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/devfs/cpp/connector.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/sync/cpp/completion.h>
#include <zircon/compiler.h>
#include <zircon/syscalls.h>

#include <cstring>
#include <deque>
#include <queue>
#include <vector>

#include <usb-endpoint/usb-endpoint-client.h>
#include <usb-inspect/usb-inspect.h>
#include <usb/descriptors.h>
#include <usb/usb.h>

namespace usb_adb_function {

class UsbAdbTestHelper;
namespace fdescriptor = fuchsia_hardware_usb_descriptor;

constexpr uint32_t kBulkTxCount = 16;
constexpr uint32_t kBulkRxCount = 16;
constexpr size_t kVmoDataSize = 4096;
constexpr size_t kScatterGatherBulkTxVmoSize = 16384;

constexpr uint16_t kBulkMaxPacket = 512;

constexpr size_t kAdbHeaderSize = 24;
constexpr uint32_t kAdbCommandConnect = 0x4e584e43;  // "CNXN"
constexpr uint32_t kAdbCommandAuth = 0x48545541;     // "AUTH"
constexpr uint32_t kAdbCommandStls = 0x534c5453;     // "STLS"
constexpr uint32_t kAdbProtocolVersionMin = 0x01000000;
constexpr uint32_t kMaxAdbPayload = 1024 * 1024;

constexpr char kDeviceName[] = "usb-adb-function";

namespace fadb = fuchsia_hardware_adb;
namespace fendpoint = fuchsia_hardware_usb_endpoint;

// The driver's internal state machine. Begins in kAwaitingUsbConnection.
enum class State : uint8_t {
  // In kAwaitingUsbConnection, we have called function_.Configure(), and
  // are waiting for usb-peripheral to call SetConfigured(true). Calls to
  // SetConfigured(false) complete immediately with ZX_OK.
  //
  // Once SetConfigured(true) has been called, we:
  // - Configure and enable the bulk endpoints,
  // - Queue USB receive requests to the bulk OUT endpoint,
  // - Tell any connected UsbAdbImpl clients that the device is online, and
  // - Move to kOnline.
  //
  // If Stop is called, we move to kStoppingForUnbind.
  kAwaitingUsbConnection,

  // In kOnline, the USB connection is live, and we respond to any
  // QueueTx/Receive requests from UsbAdbImpl clients.
  //
  // If any of the following happen:
  // - A fadb::Device client calls StopAdb().
  // - A UsbAdbImpl client closes their channel.
  // - usb-peripheral calls SetConfigured(false).
  // - The host sends an adb CNXN during an existing host session (e.g. because
  //   its adb server was restarted) while data is in flight in either
  //   direction: the client holds part of a bulk OUT packet or hasn't read
  //   everything the host sent, or data for the previous session is being
  //   sent, which the new host connection may already be reading. This
  //   includes the client's answer to a previous CNXN that it hasn't sent yet.
  //   (If nothing is in flight, we reset the session in place and remain in
  //   kOnline.)
  // - Stop gets called.
  //
  // ... we disable endpoints, cancel in-flight requests, and move to
  // kStoppingForReconnect or kStoppingForUnbind.
  kOnline,

  // In kStoppingForUnbind, we wait for all outstanding USB requests to be completed.
  // Once they have been, we close any UsbAdbImpl connection, reply to any pending
  // StopAdb()/SetConfigured(false) completers, deconfigure the USB function, and
  // complete the driver Stop callback.
  kStoppingForUnbind,

  // In kStoppingForReconnect, we wait for all outstanding USB requests to be completed.
  // Once they have been:
  // - If the stop was initiated by ADB while online (reset_usb_on_stop_ is true),
  //   we call function_.Deconfigure() and StartUsb() (function_.Configure()) to
  //   bounce the USB connection for adbd re-enumeration, then move to
  //   kAwaitingUsbConnection.
  // - If the stop was initiated by SetConfigured(false) (host reset or physical unplug),
  //   we keep usb_function_binding_ and adb_binding_ bound, notify adb_binding_
  //   that the device is offline, reply to set_configured_completers_, and move
  //   back to kAwaitingUsbConnection.
  kStoppingForReconnect,
};

// Implements the USB ADB function driver.
class UsbAdbDevice : public fdf::DriverBase2,
                     public fidl::Server<fuchsia_hardware_usb_function::UsbFunctionInterface>,
                     public fidl::WireServer<fadb::Device>,
                     public fidl::Server<fadb::UsbAdbImpl> {
 public:
  friend class UsbAdbTestHelper;
  UsbAdbDevice() : fdf::DriverBase2("usb_adb") {}

  // Driver lifecycle methods.
  zx::result<> Start(fdf::DriverContext context) override;
  void Stop(fdf::StopCompleter completer) override;

  // UsbFunctionInterface methods.
  void Control(ControlRequest& request, ControlCompleter::Sync& completer) override;
  void SetConfigured(SetConfiguredRequest& request,
                     SetConfiguredCompleter::Sync& completer) override;
  void SetInterface(SetInterfaceRequest& request, SetInterfaceCompleter::Sync& completer) override;
  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_usb_function::UsbFunctionInterface> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

  // fadb::Device methods.
  void StartAdb(StartAdbRequestView request, StartAdbCompleter::Sync& completer) override;
  void StopAdb(StopAdbCompleter::Sync& completer) override;

  // fadb::UsbAdbImpl methods.
  void QueueTx(QueueTxRequest& request, QueueTxCompleter::Sync& completer) override;
  void Receive(ReceiveCompleter::Sync& completer) override;

  uint8_t bulk_out_addr() const { return descriptors_.bulk_out_ep.b_endpoint_address; }
  uint8_t bulk_in_addr() const { return descriptors_.bulk_in_ep.b_endpoint_address; }
  inspect::Inspector GetInspectorForTesting() {
    if (component_inspector_.has_value()) {
      return component_inspector_->inspector();
    }
    return {};
  }
  usb_inspect::ThroughputTracker& GetThroughputTrackerForTesting() { return *throughput_tracker_; }

 private:
  State state_ = State::kAwaitingUsbConnection;

  // State transition helpers.
  void SetState(State new_state);
  zx::result<> StartUsb();
  zx::result<> EnableEndpoints();
  void PurgeOfflineQueues();
  void ResetOrStopUsb(State stop_state);
  void CheckUsbStopComplete();

  fidl::ServerBindingGroup<fadb::Device> device_bindings_;

  struct AdbHeader {
    uint32_t command;
    uint32_t arg0;
    uint32_t arg1;
    uint32_t data_length;
    uint32_t data_check;
    uint32_t magic;

    // FromBytes reads the header information out of a byte buffer and fixes up the endianness of
    // all fields.
    static AdbHeader FromBytes(const uint8_t* bytes) {
      AdbHeader header;

      std::memcpy(&header.command, bytes + 0, sizeof(uint32_t));
      std::memcpy(&header.arg0, bytes + 4, sizeof(uint32_t));
      std::memcpy(&header.arg1, bytes + 8, sizeof(uint32_t));
      std::memcpy(&header.data_length, bytes + 12, sizeof(uint32_t));
      std::memcpy(&header.data_check, bytes + 16, sizeof(uint32_t));
      std::memcpy(&header.magic, bytes + 20, sizeof(uint32_t));
      header.command = le32toh(header.command);
      header.arg0 = le32toh(header.arg0);
      header.arg1 = le32toh(header.arg1);
      header.data_length = le32toh(header.data_length);
      header.data_check = le32toh(header.data_check);
      header.magic = le32toh(header.magic);

      return header;
    }

    // FromVaddr reads the header information out of a DMA payload pointed at by the given vaddr and
    // fixes up the endianness of all fields.
    static AdbHeader FromVaddr(const zx_vaddr_t vaddr) {
      auto* bytes = reinterpret_cast<const uint8_t*>(vaddr);
      zx_cache_flush(bytes, kAdbHeaderSize, ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE);
      return FromBytes(bytes);
    }
  };

  // Structure to store pending and in-flight transfer requests.
  struct txn_req_t {
    QueueTxRequest request;
    size_t start = 0;
    std::vector<uint64_t> in_flight_vmo_ids;
    zx_status_t status = ZX_OK;
    QueueTxCompleter::Async completer;
  };

  // Helper methods to get free request buffer and queue the request for transmitting.
  void SendQueued();

  // Helper methods to get free request buffer and queue the request for receiving.
  void ReceiveQueued();

  // USB request completion callback methods.
  void TxComplete(std::vector<fendpoint::Completion> completion);
  void RxComplete(std::vector<fendpoint::Completion> completion);

  // Returns true if |completion| is a successful bulk OUT transfer that ends with an adb CNXN
  // packet header sent as its own USB packet, either alone or after whole packets of an interrupted
  // transfer.
  bool IsAdbConnect(const fendpoint::Completion& completion);

  // Tracks adb packet boundaries in a stream of transfers, which may split or coalesce packets.
  // Returns how many payload bytes of the current packet remain after |data|. A header split across
  // two transfers can't be parsed; tracking then resumes at the next transfer that starts with a
  // header.
  static size_t PayloadRemainingAfter(size_t remaining, const uint8_t* data, size_t length);

  // Tracks the outgoing adb packet stream while online, and returns true if |data| belongs to the
  // previous host session after an in-place session reset, and must not reach the new one.
  bool IsStaleTx(const std::vector<uint8_t>& data);

  fidl::SyncClient<fuchsia_hardware_usb_function::UsbFunction> function_;
  // Owning ServerBinding for USB function interface.
  std::optional<fidl::ServerBinding<fuchsia_hardware_usb_function::UsbFunctionInterface>>
      usb_function_binding_;

  // UsbAdbImpl service binding. ServerEnds passed into StartAdb() end up here.
  std::optional<fidl::ServerBinding<fadb::UsbAdbImpl>> adb_binding_;

  // Callbacks to call when the USB stack has been brought down and it's safe to
  // call AdbStart().
  std::vector<StopAdbCompleter::Async> stop_completers_;
  // Completers for SetConfigured(false) held until CancelAll and in-flight requests finish.
  std::vector<SetConfiguredCompleter::Async> set_configured_completers_;
  // Holds Stop callback to be invoked once shutdown is complete.
  std::optional<fdf::StopCompleter> shutdown_callback_;

  // USB ADB interface descriptor.
  struct {
    usb_interface_descriptor_t adb_intf;
    usb_endpoint_descriptor_t bulk_out_ep;
    usb_endpoint_descriptor_t bulk_in_ep;
  } descriptors_ [[maybe_unused]] = {
      .adb_intf =
          {
              .b_length = sizeof(usb_interface_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
              .b_interface_number = 0,  // set later during AllocInterface
              .b_alternate_setting = 0,
              .b_num_endpoints = 2,
              .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kVendor),
              .b_interface_sub_class =
                  fidl::ToUnderlying(fdescriptor::VendorSubclass::kAdbFastboot),
              .b_interface_protocol = fidl::ToUnderlying(fdescriptor::AdbFastbootProtocol::kAdb),
              .i_interface = 0,  // This is set in adb
          },
      .bulk_out_ep =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
              .b_endpoint_address = 0,  // set later during AllocEp
              .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
              .w_max_packet_size = htole16(kBulkMaxPacket),
              .b_interval = 0,
          },
      .bulk_in_ep =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
              .b_endpoint_address = 0,  // set later during AllocEp
              .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
              .w_max_packet_size = htole16(kBulkMaxPacket),
              .b_interval = 0,
          },
  };

  zx_status_t InitEndpoint(fidl::ClientEnd<fuchsia_hardware_usb_endpoint::Endpoint> endpoint_client,
                           usb::EndpointClient<UsbAdbDevice>& ep, uint32_t req_count,
                           size_t vmo_data_size);

  // Bulk OUT/RX endpoint
  usb::EndpointClient<UsbAdbDevice> bulk_out_ep_{usb::EndpointType::BULK, this,
                                                 std::mem_fn(&UsbAdbDevice::RxComplete)};
  // Queue of pending Receive requests from client.
  std::queue<ReceiveCompleter::Async> rx_requests_;
  // pending_replies_ only used for bulk_out_ep_
  std::queue<fendpoint::Completion> pending_replies_;

  // Bulk IN/TX endpoint
  usb::EndpointClient<UsbAdbDevice> bulk_in_ep_{usb::EndpointType::BULK, this,
                                                std::mem_fn(&UsbAdbDevice::TxComplete)};
  size_t bulk_tx_vmo_size_ = kVmoDataSize;
  // Queue of pending and in-flight transfer requests awaiting BULK IN transmission completion.
  std::deque<txn_req_t> tx_pending_reqs_;

  bool bulk_in_cancelled_ = false;
  bool bulk_out_cancelled_ = false;
  zx_status_t disable_endpoints_status_ = ZX_OK;
  bool pending_reconfigure_ = false;
  bool reset_usb_on_stop_ = false;
  bool usb_disconnect_observed_ = false;
  bool adb_connected_during_stop_ = false;

  // Whether the host has sent anything since the endpoints were last enabled.
  bool host_data_received_ = false;

  // Whether data has been queued to the host since it first sent something after the endpoints
  // were last enabled, i.e. the device has answered the host's CNXN. A CNXN received after that
  // means the host started a new adb session without a USB disconnect. Data queued before the host
  // sends anything (e.g. written by a client while offline) does not count.
  bool host_session_started_ = false;

  // Remaining payload bytes expected on bulk OUT for the current ADB packet already delivered to
  // the UsbAdbImpl client via Receive(). When non-zero, the client's packet reader is mid-packet.
  size_t rx_remaining_payload_bytes_ = 0;

  // Remaining payload bytes expected from the UsbAdbImpl client via QueueTx() for the current
  // outgoing ADB packet.
  size_t tx_remaining_payload_bytes_ = 0;

  // True after an in-place CNXN reset until the client answers the CNXN (see IsStaleTx()).
  bool awaiting_new_session_tx_ = false;

  bool CancelAllCompleted() const { return bulk_in_cancelled_ && bulk_out_cancelled_; }
  bool AllRequestsReturned() {
    bool bulk_in_ready = bulk_in_ep_.RequestsFull() || !bulk_in_ep_.client().is_valid();
    bool bulk_out_ready = bulk_out_ep_.RequestsFull() || !bulk_out_ep_.client().is_valid();
    return bulk_in_ready && bulk_out_ready;
  }

  // Inspect diagnostics
  std::optional<inspect::ComponentInspector> component_inspector_;
  inspect::Node inspect_node_;
  usb_inspect::FunctionDriverInspect function_inspect_;
  usb_inspect::EndpointInspect bulk_in_inspect_;
  usb_inspect::EndpointInspect bulk_out_inspect_;

  void RecordEvent(std::string_view event_name) { function_inspect_.RecordEvent(event_name); }
  void UpdateQueueStats() {
    bulk_in_inspect_.UpdateTxQueue(tx_pending_reqs_.size());
    bulk_out_inspect_.UpdateRxQueue(rx_requests_.size());
    bulk_out_inspect_.UpdateRxPendingProcessing(pending_replies_.size());
  }

  // Traffic and throughput calculation
  std::optional<usb_inspect::ThroughputTracker> throughput_tracker_;

  static std::string StateToString(State state) {
    switch (state) {
      case State::kAwaitingUsbConnection:
        return "kAwaitingUsbConnection";
      case State::kOnline:
        return "kOnline";
      case State::kStoppingForUnbind:
        return "kStoppingForUnbind";
      case State::kStoppingForReconnect:
        return "kStoppingForReconnect";
    }
    return "unknown";
  }
};

}  // namespace usb_adb_function

#endif  // SRC_DEVELOPER_ADB_DRIVERS_USB_ADB_FUNCTION_ADB_FUNCTION_H_
