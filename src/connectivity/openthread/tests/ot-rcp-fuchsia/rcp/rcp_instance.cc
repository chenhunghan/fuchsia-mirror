// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/rcp_instance.h"

#include <lib/syslog/cpp/macros.h>
#include <zircon/assert.h>

#include <ios>
#include <utility>

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/ncp_fidl.h"

#include "instance/instance.hpp"

namespace ot_rcp {

RcpInstance::RcpInstance(async_dispatcher_t* dispatcher, uint32_t node_id, RcpToHostFn rcp_to_host)
    : dispatcher_(dispatcher), node_id_(node_id), rcp_to_host_(std::move(rcp_to_host)) {
  platform_.emplace(dispatcher_, node_id_);
  FX_LOGS(INFO) << "Starting ot-rcp-fuchsia, node id " << node_id_ << ", EUI-64 0x" << std::hex
                << platform_->eui64();
}

RcpInstance::~RcpInstance() { Teardown(); }

void RcpInstance::Teardown() {
  // `otInstanceFinalize()` still calls into the platform (e.g. to stop alarms), so the platform
  // must outlive it.
  ncp_.reset();
  if (instance_ != nullptr) {
    otInstanceFinalize(instance_);
    instance_ = nullptr;
  }
  platform_.reset();
}

void RcpInstance::Reset(otPlatResetReason reason) {
  Teardown();
  // `Teardown()` destroyed the Platform; a fresh one starts from the power-on configuration.
  platform_.emplace(dispatcher_, node_id_);
  platform_->set_reset_reason(reason);

  instance_ = otInstanceInitSingle();
  FX_CHECK(instance_ != nullptr && otInstanceIsInitialized(instance_))
      << "otInstanceInitSingle failed";
  // NcpAdapter owns a forwarder to `rcp_to_host_`.
  ncp_ = std::make_unique<ot::Ncp::NcpAdapter>(
      ot::AsCoreTypePtr(instance_),
      [this](const uint8_t* frame, uint16_t length) { return rcp_to_host_(frame, length); });
  FX_LOGS(INFO) << "RCP instance (re)initialized, reset reason " << static_cast<int>(reason);
}

void RcpInstance::HostToRcp(const uint8_t* frame, uint16_t length) {
  ZX_ASSERT_MSG(ncp_ != nullptr, "HostToRcp() called while not running");
  ncp_->Receive(frame, length);
}

void RcpInstance::Kick() {
  ZX_ASSERT_MSG(ncp_ != nullptr, "Kick() called while not running");
  ncp_->Kick();
}

bool RcpInstance::HasPendingClientboundFrames() const {
  return ncp_ != nullptr && ncp_->HasPendingClientboundFrames();
}

}  // namespace ot_rcp
