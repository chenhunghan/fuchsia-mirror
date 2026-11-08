// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/spinel_device_server.h"

#include <lib/syslog/cpp/macros.h>
#include <zircon/assert.h>

#include <limits>
#include <memory>
#include <utility>

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/ncp_fidl.h"

namespace ot_rcp {

namespace fspinel = fuchsia_lowpan_spinel;

// RCP-to-host frame should fit within the size advertised by `GetMaxFrameSize()`.
static_assert(ot::Ncp::NcpAdapter::kMaxFrameSize <= SpinelDeviceServer::kMaxFrameSize,
              "RCP-to-host frames may exceed the advertised max frame size");
// Accepted host frames are passed to `NcpBase` with a `uint16_t` length.
static_assert(SpinelDeviceServer::kMaxFrameSize <= std::numeric_limits<uint16_t>::max(),
              "kMaxFrameSize must fit the uint16_t frame length used by NcpBase");

class SpinelDeviceServer::DeviceConnection : public fidl::Server<fspinel::Device> {
 public:
  DeviceConnection(SpinelDeviceServer& server, async_dispatcher_t* dispatcher,
                   fidl::ServerEnd<fspinel::Device> server_end)
      : server_(server),
        binding_(dispatcher, std::move(server_end), this,
                 [&server](fidl::UnbindInfo info) { server.OnDeviceClosed(info); }) {}

  void Open(OpenCompleter::Sync& completer) override {
    server_.Open();
    completer.Reply(fit::ok());
  }

  void Close(CloseCompleter::Sync& completer) override {
    server_.Close();
    completer.Reply(fit::ok());
  }

  void GetMaxFrameSize(GetMaxFrameSizeCompleter::Sync& completer) override {
    completer.Reply(fspinel::DeviceGetMaxFrameSizeResponse(kMaxFrameSize));
  }

  void SendFrame(SendFrameRequest& request, SendFrameCompleter::Sync& completer) override {
    server_.SendFrame(std::move(request.data()));
  }

  void ReadyToReceiveFrames(ReadyToReceiveFramesRequest& request,
                            ReadyToReceiveFramesCompleter::Sync& completer) override {
    server_.ReadyToReceiveFrames(request.number_of_frames());
  }

  fidl::ServerBinding<fspinel::Device>& binding() { return binding_; }

 private:
  SpinelDeviceServer& server_;
  fidl::ServerBinding<fspinel::Device> binding_;
};

SpinelDeviceServer::SpinelDeviceServer(async_dispatcher_t* dispatcher, uint32_t node_id)
    : SpinelDeviceServer(dispatcher, node_id, FlowControl{}) {}

SpinelDeviceServer::SpinelDeviceServer(async_dispatcher_t* dispatcher, uint32_t node_id,
                                       FlowControl flow_control)
    : dispatcher_(dispatcher),
      flow_control_(flow_control),
      rcp_(dispatcher, node_id,
           [this](const uint8_t* frame, uint16_t length) { return RcpToHost(frame, length); }) {
  ZX_ASSERT_MSG(flow_control_.radiobound_credit_inc > 0, "radiobound_credit_inc must be non-zero");
}

SpinelDeviceServer::~SpinelDeviceServer() = default;

fspinel::Service::InstanceHandler SpinelDeviceServer::GetInstanceHandler() {
  return fspinel::Service::InstanceHandler({
      .device_setup = setup_bindings_.CreateHandler(this, dispatcher_, fidl::kIgnoreBindingClosure),
  });
}

void SpinelDeviceServer::SetChannel(SetChannelRequest& request,
                                    SetChannelCompleter::Sync& completer) {
  // The previous Device connection is only released once its close has been handled
  // (`OnDeviceClosed()`). A new client racing that close gets ALREADY_BOUND and should retry.
  if (device_ != nullptr) {
    FX_LOGS(WARNING) << "SetChannel: a Device channel is already bound";
    completer.Reply(fit::error(ZX_ERR_ALREADY_BOUND));
    return;
  }

  device_ = std::make_unique<DeviceConnection>(*this, dispatcher_, std::move(request.req()));
  FX_LOGS(INFO) << "SetChannel: Device channel bound";
  completer.Reply(fit::ok());
}

void SpinelDeviceServer::OnDeviceClosed(fidl::UnbindInfo info) {
  FX_LOGS(INFO) << "Device channel closed: " << info.FormatDescription();
  // The binding has already torn down; destroying it here is safe and discards nothing.
  device_.reset();
  CloseDevice();
}

void SpinelDeviceServer::CloseDevice() {
  open_ = false;
  clientbound_credits_ = 0;
  radiobound_credits_ = 0;
  radiobound_consumed_ = 0;
  radiobound_queue_.clear();
  process_radiobound_task_.Cancel();
  // Drops all undelivered RCP-to-host frames. Nothing to reset if the device was never opened.
  if (rcp_.is_running()) {
    rcp_.Reset(OT_PLAT_RESET_REASON_POWER_ON);
  }
}

void SpinelDeviceServer::Open() {
  // Opening the device resets the RCP.
  rcp_.Reset(OT_PLAT_RESET_REASON_POWER_ON);
  process_radiobound_task_.Cancel();
  radiobound_queue_.clear();
  radiobound_consumed_ = 0;
  clientbound_credits_ = 0;
  radiobound_credits_ = flow_control_.radiobound_credit_init;
  open_ = true;
  FX_LOGS(INFO) << "Device opened";
  SendOnReadyForSendFrames(flow_control_.radiobound_credit_init);
}

void SpinelDeviceServer::Close() {
  FX_LOGS(INFO) << "Device closed";
  CloseDevice();
}

void SpinelDeviceServer::SendFrame(std::vector<uint8_t> data) {
  if (!open_) {
    SendOnError(fspinel::Error::kClosed);
    return;
  }
  if (data.size() > kMaxFrameSize) {
    SendOnError(fspinel::Error::kOutboundFrameTooLarge);
    return;
  }
  if (radiobound_credits_ == 0) {
    // The client violated the flow control protocol: close the device and the channel. The
    // connection is released by `OnDeviceClosed()` once the binding has torn down.
    FX_LOGS(WARNING) << "SendFrame without radio-bound credit; closing channel with IO_OVERRUN";
    CloseDevice();
    device_->binding().Close(ZX_ERR_IO_OVERRUN);
    return;
  }
  radiobound_credits_--;
  radiobound_queue_.push_back(std::move(data));
  ProcessRadioboundQueue();
}

void SpinelDeviceServer::ReadyToReceiveFrames(uint32_t number_of_frames) {
  if (number_of_frames == 0 || !open_) {
    return;
  }
  // Uncommon use case: a misbehaving host granting more than UINT32_MAX frames in
  // total must not end up with 0 credits.
  clientbound_credits_ =
      number_of_frames > std::numeric_limits<uint32_t>::max() - clientbound_credits_
          ? std::numeric_limits<uint32_t>::max()
          : clientbound_credits_ + number_of_frames;
  rcp_.Kick();
  ProcessRadioboundQueue();
}

bool SpinelDeviceServer::IsBlockedOnHost() const {
  return clientbound_credits_ == 0 && rcp_.HasPendingClientboundFrames();
}

void SpinelDeviceServer::ProcessRadioboundQueue() {
  while (open_ && rcp_.is_running() && !radiobound_queue_.empty() && !IsBlockedOnHost()) {
    std::vector<uint8_t> frame = std::move(radiobound_queue_.front());
    radiobound_queue_.pop_front();
    rcp_.HostToRcp(frame.data(), static_cast<uint16_t>(frame.size()));
    radiobound_consumed_++;
    if (radiobound_consumed_ % flow_control_.radiobound_credit_inc == 0) {
      radiobound_credits_ += flow_control_.radiobound_credit_inc;
      SendOnReadyForSendFrames(flow_control_.radiobound_credit_inc);
    }
  }
}

bool SpinelDeviceServer::RcpToHost(const uint8_t* frame, uint16_t length) {
  if (!open_ || device_ == nullptr || clientbound_credits_ == 0) {
    return false;
  }
  fit::result result = fidl::SendEvent(device_->binding())
                           ->OnReceiveFrame(fspinel::DeviceOnReceiveFrameRequest(
                               std::vector<uint8_t>(frame, frame + length)));
  if (result.is_error()) {
    FX_LOGS(WARNING) << "Failed to send OnReceiveFrame: " << result.error_value();
    return false;
  }
  clientbound_credits_--;
  // Delivering frames may unblock queued radio-bound frames.
  if (!radiobound_queue_.empty() && !process_radiobound_task_.is_pending()) {
    process_radiobound_task_.Post(dispatcher_);
  }
  return true;
}

void SpinelDeviceServer::SendOnReadyForSendFrames(uint32_t number_of_frames) {
  if (device_ == nullptr) {
    return;
  }
  fit::result result =
      fidl::SendEvent(device_->binding())
          ->OnReadyForSendFrames(fspinel::DeviceOnReadyForSendFramesRequest(number_of_frames));
  if (result.is_error()) {
    FX_LOGS(WARNING) << "Failed to send OnReadyForSendFrames: " << result.error_value();
  }
}

void SpinelDeviceServer::SendOnError(fspinel::Error error) {
  if (device_ == nullptr) {
    return;
  }
  fit::result result = fidl::SendEvent(device_->binding())
                           ->OnError(fspinel::DeviceOnErrorRequest(error, /*did_close=*/false));
  if (result.is_error()) {
    FX_LOGS(WARNING) << "Failed to send OnError: " << result.error_value();
  }
}

}  // namespace ot_rcp
