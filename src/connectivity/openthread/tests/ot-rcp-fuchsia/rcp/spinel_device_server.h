// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_SPINEL_DEVICE_SERVER_H_
#define SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_SPINEL_DEVICE_SERVER_H_

#include <fidl/fuchsia.lowpan.spinel/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/async/dispatcher.h>

#include <cstdint>
#include <deque>
#include <memory>
#include <vector>

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/rcp_instance.h"

namespace ot_rcp {

// Serves `fuchsia.lowpan.spinel` FIDL on top of an RcpInstance.
class SpinelDeviceServer : public fidl::Server<fuchsia_lowpan_spinel::DeviceSetup> {
 public:
  // Largest spinel frame accepted.
  static constexpr uint32_t kMaxFrameSize = 2048;

  // Radio-bound (host to RCP) credit parameters.
  struct FlowControl {
    // Credits granted by `Open()`.
    uint32_t radiobound_credit_init = 4;
    // Credits re-granted for every this many frames consumed by `NcpBase`. Must be non-zero.
    uint32_t radiobound_credit_inc = 2;
  };

  // `node_id` selects the RCP's EUI-64 and log tag.
  SpinelDeviceServer(async_dispatcher_t* dispatcher, uint32_t node_id);
  SpinelDeviceServer(async_dispatcher_t* dispatcher, uint32_t node_id, FlowControl flow_control);
  ~SpinelDeviceServer() override;

  const FlowControl& flow_control() const { return flow_control_; }

  // Handler for the `device_setup` member of fuchsia.lowpan.spinel.Service.
  fuchsia_lowpan_spinel::Service::InstanceHandler GetInstanceHandler();

  // Binds `request.req()` as the Device channel. Fails with ZX_ERR_ALREADY_BOUND while a previous
  // Device channel is bound, including one whose peer has closed but whose close the server has
  // not handled yet; a client reconnecting after dropping its channel should retry.
  void SetChannel(SetChannelRequest& request, SetChannelCompleter::Sync& completer) override;

 private:
  // One bound Device channel: the fidl::Server<Device> for it plus the binding that owns it.
  class DeviceConnection;

  // fuchsia.lowpan.spinel/Device, dispatched from DeviceConnection.
  void Open();
  void Close();
  void SendFrame(std::vector<uint8_t> data);
  void ReadyToReceiveFrames(uint32_t number_of_frames);

  // Close handler of the Device binding (peer closed, protocol error, or our own Close()).
  void OnDeviceClosed(fidl::UnbindInfo info);
  // Marks the device closed, drops queued radio-bound frames and soft-resets the RCP.
  void CloseDevice();

  // Feeds queued radio-bound frames into the RCP while it is not blocked on the host.
  void ProcessRadioboundQueue();
  bool IsBlockedOnHost() const;

  // RcpInstance::RcpToHostFn: delivers one frame to the host as OnReceiveFrame, or returns false.
  bool RcpToHost(const uint8_t* frame, uint16_t length);

  void SendOnReadyForSendFrames(uint32_t number_of_frames);
  void SendOnError(fuchsia_lowpan_spinel::Error error);

  async_dispatcher_t* const dispatcher_;
  const FlowControl flow_control_;
  RcpInstance rcp_;
  fidl::ServerBindingGroup<fuchsia_lowpan_spinel::DeviceSetup> setup_bindings_;

  // The bound Device channel, if any. Owning: destroying it tears the binding down without
  // invoking OnDeviceClosed(), so no callback can outlive this object. Reset only by
  // OnDeviceClosed(); until then SetChannel() answers ALREADY_BOUND.
  std::unique_ptr<DeviceConnection> device_;

  bool open_ = false;
  uint32_t clientbound_credits_ = 0;
  uint32_t radiobound_credits_ = 0;
  uint32_t radiobound_consumed_ = 0;
  std::deque<std::vector<uint8_t>> radiobound_queue_;

  async::TaskClosureMethod<SpinelDeviceServer, &SpinelDeviceServer::ProcessRadioboundQueue>
      process_radiobound_task_{this};
};

}  // namespace ot_rcp

#endif  // SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_SPINEL_DEVICE_SERVER_H_
