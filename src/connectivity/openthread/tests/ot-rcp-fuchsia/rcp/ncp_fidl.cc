// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/ncp_fidl.h"

#include <utility>

#include "common/code_utils.hpp"
#include "instance/instance.hpp"

namespace ot {
namespace Ncp {
NcpAdapter::NcpAdapter(Instance* instance, SendFn send)
    : NcpBase(instance), mSend(std::move(send)), mSendTask(*instance, HandleSendTask), mFrame{} {
  mTxFrameBuffer.SetFrameAddedCallback(HandleFrameAddedToNcpBuffer, this);
}

// `NcpBase` in OpenThread sets this, but it has no destructor, thus we handle from platform side.
NcpAdapter::~NcpAdapter() {
  if (sNcpInstance == this) {
    sNcpInstance = nullptr;
  }
}

void NcpAdapter::HandleFrameAddedToNcpBuffer(void* context, Spinel::Buffer::FrameTag tag,
                                             Spinel::Buffer::Priority priority,
                                             Spinel::Buffer* buffer) {
  static_cast<NcpAdapter*>(context)->Kick();
}

void NcpAdapter::HandleSendTask(Tasklet& tasklet) {
  NcpAdapter* ncp = static_cast<NcpAdapter*>(GetNcpInstance());
  if (ncp != nullptr) {
    ncp->Drain();
  }
}

void NcpAdapter::Drain() {
  while (!mTxFrameBuffer.IsEmpty()) {
    if (ShouldDeferHostSend()) {
      break;
    }
    // Rewinds to the start of the oldest frame, so a frame that could not be delivered on a
    // previous pass is re-read in full.
    if (mTxFrameBuffer.OutFrameBegin() != OT_ERROR_NONE) {
      break;
    }
    const uint16_t length = mTxFrameBuffer.OutFrameGetLength();
    const uint16_t read = mTxFrameBuffer.OutFrameRead(length, mFrame);
    // Attempt to send to host. If there's no RCP to host allowance or device not open, leave
    // the frame queued and retry in the next `Kick()`.
    if (!mSend(mFrame, read)) {
      break;
    }
    IgnoreError(mTxFrameBuffer.OutFrameRemove());
  }
}

}  // namespace Ncp
}  // namespace ot
