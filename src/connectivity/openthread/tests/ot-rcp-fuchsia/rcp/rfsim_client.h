// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_RFSIM_CLIENT_H_
#define SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_RFSIM_CLIENT_H_

#include <fidl/test.lowpan.rfsim/cpp/fidl.h>
#include <lib/async/dispatcher.h>
#include <lib/fit/function.h>

#include <cstdint>
#include <memory>
#include <optional>

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/platform/radio.h"

namespace ot_rcp {

// The node's connection to the simulated rf medium (test.lowpan.rfsim.Medium).
//
// `Attach()` attaches a node to the Medium with a new Radio channel and hands over the client
// once Attach succeeds, so an RfsimClient is always attached. From then on:
//  * the radio's transmissions are forwarded over `Radio.Transmit`, one at a time: `done` runs when
//    the medium replies, which is what lets the radio report TxDone and transmit the next frame;
//  * once a receiver is first set, exactly one `Radio.Receive` is outstanding, and every frame it
//    returns goes to the current receiver. Until then, the medium queues the node's frames.
class RfsimClient : public RadioMedium, public fidl::AsyncEventHandler<test_lowpan_rfsim::Radio> {
 public:
  using AttachedFn = fit::callback<void(std::unique_ptr<RfsimClient>)>;

  // Connects to the Medium in the component's namespace and attaches `node_id` asynchronously.
  static void Attach(async_dispatcher_t* dispatcher, uint32_t node_id, AttachedFn on_attached);
  // Same, over an already connected `medium` (e.g. a medium served on the same loop).
  static void Attach(async_dispatcher_t* dispatcher, uint32_t node_id,
                     fidl::ClientEnd<test_lowpan_rfsim::Medium> medium, AttachedFn on_attached);

  ~RfsimClient() override = default;

  RfsimClient(const RfsimClient&) = delete;
  RfsimClient& operator=(const RfsimClient&) = delete;

  // RadioMedium. Calling again before `done` has run is a programming error.
  void Transmit(const MediumFrame& frame, int8_t tx_power_dbm, DoneFn done) override;
  // RadioMedium. The first non-null receiver starts receiving.
  void SetReceiver(RadioReceiver* receiver) override;

  // fidl::AsyncEventHandler<Radio>
  void on_fidl_error(fidl::UnbindInfo error) override;

 private:
  explicit RfsimClient(uint32_t node_id);

  // Calls `Radio.Receive`; its reply goes to the receiver and calls this again.
  void Receive();

  const uint32_t node_id_;
  bool transmit_pending_ = false;
  bool receiving_ = false;
  RadioReceiver* receiver_ = nullptr;
  // The Medium the node attached to. Closing `radio_` detaches the node.
  std::optional<fidl::Client<test_lowpan_rfsim::Medium>> medium_;
  std::optional<fidl::Client<test_lowpan_rfsim::Radio>> radio_;
};

}  // namespace ot_rcp

#endif  // SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_RFSIM_CLIENT_H_
