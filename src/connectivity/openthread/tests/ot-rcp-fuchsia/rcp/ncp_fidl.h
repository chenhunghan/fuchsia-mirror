// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_NCP_FIDL_H_
#define SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_NCP_FIDL_H_

#include <lib/fit/function.h>

#include <cstdint>

#include "common/tasklet.hpp"
#include "ncp/ncp_base.hpp"

namespace ot {
namespace Ncp {

// `NcpBase` adapter for Spinel frames carried over `fuchsia.lowpan.spinel`.
// The `NcpBase` is implemented in `src/ncp:libopenthread-rcp` in OpenThread.
class NcpAdapter : public NcpBase {
 public:
  // Returns true if the frame was delivered to the host, false if it must stay queued.
  using SendFn = fit::function<bool(const uint8_t* frame, uint16_t length)>;

  // Largest frame `NcpBase` can produce.
  static constexpr uint16_t kMaxFrameSize = OPENTHREAD_CONFIG_NCP_TX_BUFFER_SIZE;

  NcpAdapter(Instance* instance, SendFn send);

  ~NcpAdapter();
  NcpAdapter(const NcpAdapter&) = delete;
  NcpAdapter& operator=(const NcpAdapter&) = delete;

  // Feeds outbound host-to-RCP Spinel frame into `NcpBase`.
  void Receive(const uint8_t* frame, uint16_t length) { HandleReceive(frame, length); }

  // Posts a task to deliver inbound RCP-to-host frames to lowpan.
  void Kick() { mSendTask.Post(); }

  // True if `NcpBase` has RCP-to-host frames that have not been delivered yet.
  bool HasPendingClientboundFrames() const {
    return !mTxFrameBuffer.IsEmpty() || !IsResponseQueueEmpty() || !mChangedPropsSet.IsEmpty();
  }

 private:
  static void HandleFrameAddedToNcpBuffer(void* context, Spinel::Buffer::FrameTag tag,
                                          Spinel::Buffer::Priority priority,
                                          Spinel::Buffer* buffer);
  static void HandleSendTask(Tasklet& tasklet);
  void Drain();

  SendFn mSend;
  Tasklet mSendTask;
  uint8_t mFrame[kMaxFrameSize];
};

}  // namespace Ncp
}  // namespace ot

#endif  // SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_NCP_FIDL_H_
