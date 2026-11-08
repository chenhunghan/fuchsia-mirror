// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_RCP_INSTANCE_H_
#define SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_RCP_INSTANCE_H_

#include <lib/async/dispatcher.h>
#include <lib/fit/function.h>

#include <cstdint>
#include <memory>
#include <optional>

#include <openthread/instance.h>
#include <openthread/platform/misc.h>

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/platform/platform.h"

namespace ot::Ncp {
class NcpAdapter;
}

namespace ot_rcp {
class RcpInstance {
 public:
  // Delivers one RCP-to-host frame. Returns false if the frame cannot be delivered now (it then
  // stays queued in `NcpBase` until `Kick()`).
  using RcpToHostFn = fit::function<bool(const uint8_t* frame, uint16_t length)>;

  // Creates the platform on `dispatcher` for `node_id`. The OpenThread instance is not started
  // until the first Reset().
  RcpInstance(async_dispatcher_t* dispatcher, uint32_t node_id, RcpToHostFn rcp_to_host);
  ~RcpInstance();

  RcpInstance(const RcpInstance&) = delete;
  RcpInstance& operator=(const RcpInstance&) = delete;

  void Reset(otPlatResetReason reason);

  // Passes a host-to-RCP Spinel frame to `NcpBase`. Only valid while `is_running()`.
  void HostToRcp(const uint8_t* frame, uint16_t length);

  // Retries delivery of queued RCP-to-host frames. Only valid while `is_running()`.
  void Kick();

  // True if `NcpBase` holds RCP-to-host frames that were not delivered yet.
  bool HasPendingClientboundFrames() const;

  bool is_running() const { return ncp_ != nullptr; }

 private:
  void Teardown();

  async_dispatcher_t* const dispatcher_;
  const uint32_t node_id_;
  RcpToHostFn rcp_to_host_;
  // Re-created on every `Reset()`, which returns the radio and alarms to their power-on state.
  std::optional<Platform> platform_;
  otInstance* instance_ = nullptr;
  std::unique_ptr<ot::Ncp::NcpAdapter> ncp_;
};

}  // namespace ot_rcp

#endif  // SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_RCP_INSTANCE_H_
