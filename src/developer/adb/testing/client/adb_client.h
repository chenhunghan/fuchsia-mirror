// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_ADB_TESTING_CLIENT_ADB_CLIENT_H_
#define SRC_DEVELOPER_ADB_TESTING_CLIENT_ADB_CLIENT_H_

#include <fidl/fuchsia.hardware.usb.device/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.endpoint/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb/cpp/fidl.h>
#include <fidl/fuchsia.testing.adb/cpp/fidl.h>
#include <lib/async/dispatcher.h>
#include <lib/zx/vmo.h>

#include <cstddef>
#include <cstdint>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

#include "src/developer/adb/third_party/adb/adb-protocol.h"

class AdbClientImpl : public fidl::Server<fuchsia_testing_adb::Client>,
                      public fidl::AsyncEventHandler<fuchsia_hardware_usb_endpoint::Endpoint> {
 public:
  explicit AdbClientImpl(async_dispatcher_t* dispatcher);

  void Setup(SetupCompleter::Sync& completer) override;
  void Connect(ConnectCompleter::Sync& completer) override;
  void ExecuteCommand(ExecuteCommandRequest& request,
                      ExecuteCommandCompleter::Sync& completer) override;
  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_testing_adb::Client> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override;

  void OnCompletion(
      fidl::Event<fuchsia_hardware_usb_endpoint::Endpoint::OnCompletion>& event) override;
  void on_fidl_error(fidl::UnbindInfo error) override;

  // Binds endpoint channels and the bulk IN VMO directly for unit testing.
  void SetEndpointsForTesting(fidl::ClientEnd<fuchsia_hardware_usb_endpoint::Endpoint> bulk_in,
                              fidl::ClientEnd<fuchsia_hardware_usb_endpoint::Endpoint> bulk_out,
                              zx::vmo read_vmo);
  void UnbindBulkOutForTesting() { bulk_out_ = {}; }

 private:
  class BulkOutEventHandler
      : public fidl::AsyncEventHandler<fuchsia_hardware_usb_endpoint::Endpoint> {
   public:
    explicit BulkOutEventHandler(AdbClientImpl* parent) : parent_(parent) {}

    void OnCompletion(
        fidl::Event<fuchsia_hardware_usb_endpoint::Endpoint::OnCompletion>& event) override;
    void on_fidl_error(fidl::UnbindInfo error) override;

   private:
    AdbClientImpl* parent_;
  };

  static constexpr uint64_t kBulkReadVmoId = 1;
  static constexpr size_t kBulkReadSize = 16384;

  enum class PayloadMode : uint8_t {
    kNone,
    kConnect,
    kWrite,
    kDiscard,
  };

  void ResetPacketState();
  zx_status_t DiscoverAndConnect();
  zx_status_t ProcessDevice(fidl::SyncClient<fuchsia_hardware_usb_device::Device>& device);
  zx_status_t FindAdbInterface(const uint8_t* data, size_t len);
  zx_status_t ConnectEndpoints();
  zx_status_t SendPacket(apacket* p);
  zx_status_t SendOkay();
  zx_status_t QueueReadRequest();
  void ResetSessionState(zx_status_t status);

  async_dispatcher_t* dispatcher_;
  bool usb_connected_ = false;
  bool handshake_complete_ = false;
  std::optional<ConnectCompleter::Async> connect_completer_;
  uint8_t bulk_in_addr_ = 0;
  uint8_t bulk_out_addr_ = 0;
  BulkOutEventHandler bulk_out_handler_{this};
  fidl::Client<fuchsia_hardware_usb_endpoint::Endpoint> bulk_in_;
  fidl::Client<fuchsia_hardware_usb_endpoint::Endpoint> bulk_out_;

  std::optional<ExecuteCommandCompleter::Async> execute_completer_;
  std::string command_output_;
  uint32_t local_id_ = 1;
  uint32_t remote_id_ = 0;
  PayloadMode payload_mode_ = PayloadMode::kNone;
  size_t remaining_payload_bytes_ = 0;
  bool read_request_pending_ = false;
  zx::vmo read_vmo_;
};

#endif  // SRC_DEVELOPER_ADB_TESTING_CLIENT_ADB_CLIENT_H_
