// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/rfsim_client.h"

#include <lib/component/incoming/cpp/protocol.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/assert.h>

#include <utility>
#include <vector>

namespace ot_rcp {

namespace rfsim = test_lowpan_rfsim;

RfsimClient::RfsimClient(uint32_t node_id) : node_id_(node_id) {}

void RfsimClient::Attach(async_dispatcher_t* dispatcher, uint32_t node_id, AttachedFn on_attached) {
  zx::result medium_end = component::Connect<rfsim::Medium>();
  FX_CHECK(medium_end.is_ok()) << "Failed to connect to test.lowpan.rfsim.Medium: "
                               << medium_end.status_string();
  Attach(dispatcher, node_id, std::move(*medium_end), std::move(on_attached));
}

void RfsimClient::Attach(async_dispatcher_t* dispatcher, uint32_t node_id,
                         fidl::ClientEnd<rfsim::Medium> medium, AttachedFn on_attached) {
  // The constructor is private, so std::make_unique cannot reach it.
  std::unique_ptr<RfsimClient> client(new RfsimClient(node_id));
  auto [radio_client_end, radio_server_end] = fidl::Endpoints<rfsim::Radio>::Create();
  client->medium_.emplace(std::move(medium), dispatcher);
  client->radio_.emplace(std::move(radio_client_end), dispatcher, client.get());

  RfsimClient& self = *client;
  (*self.medium_)
      ->Attach(rfsim::MediumAttachRequest(node_id, std::move(radio_server_end)))
      .Then([client = std::move(client), on_attached = std::move(on_attached)](
                fidl::Result<rfsim::Medium::Attach>& result) mutable {
        // A medium that is not routed shows up here as PEER_CLOSED.
        FX_CHECK(result.is_ok()) << "test.lowpan.rfsim.Medium/Attach(node " << client->node_id_
                                 << ") failed: " << result.error_value().FormatDescription();
        FX_LOGS(INFO) << "Attached node " << client->node_id_ << " to test.lowpan.rfsim.Medium";
        on_attached(std::move(client));
      });
}

void RfsimClient::SetReceiver(RadioReceiver* receiver) {
  receiver_ = receiver;
  if (receiver_ != nullptr && !receiving_) {
    receiving_ = true;
    Receive();
  }
}

void RfsimClient::Receive() {
  ZX_ASSERT(radio_.has_value());
  (*radio_)->Receive().Then([this](fidl::Result<rfsim::Radio::Receive>& result) {
    FX_CHECK(result.is_ok()) << "Radio.Receive(node " << node_id_
                             << ") failed: " << result.error_value().FormatDescription();
    // The receiver is only ever null while the radio is being replaced, which does not return to
    // the loop.
    ZX_ASSERT_MSG(receiver_ != nullptr, "node %u received a frame with no receiver", node_id_);
    const rfsim::Frame& frame = result->frame();
    receiver_->HandleReceivedFrame({
        .frame = {.channel = frame.channel(), .psdu = frame.psdu()},
        .rssi_dbm = result->rssi_dbm(),
        .lqi = result->lqi(),
    });
    Receive();
  });
}

void RfsimClient::Transmit(const MediumFrame& frame, int8_t tx_power_dbm, DoneFn done) {
  FX_CHECK(!transmit_pending_) << "Radio.Transmit called while one is outstanding";
  transmit_pending_ = true;
  (*radio_)
      ->Transmit({{
          .channel = frame.channel,
          .psdu = std::vector<uint8_t>(frame.psdu.begin(), frame.psdu.end()),
          .tx_power_dbm = tx_power_dbm,
      }})
      .Then([this, done = std::move(done)](fidl::Result<rfsim::Radio::Transmit>& result) mutable {
        transmit_pending_ = false;
        FX_CHECK(result.is_ok()) << "Radio.Transmit failed: "
                                 << result.error_value().FormatDescription();
        done();
      });
}

void RfsimClient::on_fidl_error(fidl::UnbindInfo error) {
  FX_LOGS(FATAL) << "test.lowpan.rfsim.Radio channel closed: " << error.FormatDescription();
}

}  // namespace ot_rcp
