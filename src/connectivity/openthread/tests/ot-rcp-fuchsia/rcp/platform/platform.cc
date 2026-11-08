// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/platform/platform.h"

#include <lib/async/cpp/time.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/assert.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>

#include <algorithm>

#include <openthread/platform/alarm-milli.h>
#include <openthread/tasklet.h>

namespace ot_rcp {

Platform* Platform::current_ = nullptr;

Platform& Platform::Get() {
  ZX_ASSERT_MSG(current_ != nullptr, "no live Platform");
  return *current_;
}

Platform::Platform(async_dispatcher_t* dispatcher, uint32_t node_id)
    : dispatcher_(dispatcher), node_id_(node_id) {
  ZX_ASSERT_MSG(current_ == nullptr, "only one Platform may exist at a time");
  current_ = this;
  tasklet_task_.set_handler([this]() {
    if (IsLive(tasklet_instance_)) {
      otTaskletsProcess(tasklet_instance_);
    }
  });
  milli_task_.set_handler([this]() {
    if (IsLive(milli_instance_)) {
      otPlatAlarmMilliFired(milli_instance_);
    }
  });
}

Platform::~Platform() {
  // Cancel explicitly so that nothing posted to the dispatcher can reach this object after it is
  // gone; the TaskClosure destructors would do the same, but this keeps the intent visible.
  milli_task_.Cancel();
  tasklet_task_.Cancel();
  current_ = nullptr;
}

void Platform::SignalTaskletPending(otInstance* instance) {
  // A single otInstance should be used for the lifetime of a Platform.
  ZX_ASSERT_MSG(tasklet_instance_ == nullptr || tasklet_instance_ == instance,
                "otInstance changed without a new Platform");
  tasklet_instance_ = instance;
  if (tasklet_task_.is_pending()) {
    return;
  }
  zx_status_t status = tasklet_task_.Post(dispatcher_);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to post tasklet task: " << zx_status_get_string(status);
  }
}

void Platform::StartMilliAlarm(otInstance* instance, uint32_t t0, uint32_t dt) {
  ZX_ASSERT_MSG(milli_instance_ == nullptr || milli_instance_ == instance,
                "otInstance changed without a new Platform");
  milli_instance_ = instance;
  milli_task_.Cancel();

  const int64_t now_ns = NowNanos();
  const uint32_t now_ms = static_cast<uint32_t>(now_ns / ZX_MSEC(1));
  const int32_t remaining_ms = std::max<int32_t>(static_cast<int32_t>(t0 + dt - now_ms), 0);
  const zx::time deadline(now_ns + ZX_MSEC(remaining_ms));
  zx_status_t status = milli_task_.PostForTime(dispatcher_, deadline);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to post alarm task: " << zx_status_get_string(status);
  }
}

void Platform::StopMilliAlarm() {
  milli_task_.Cancel();
  milli_instance_ = nullptr;
}

uint32_t Platform::NowMillis() const { return static_cast<uint32_t>(NowNanos() / ZX_MSEC(1)); }

uint64_t Platform::NowMicros() const { return static_cast<uint64_t>(NowNanos() / ZX_USEC(1)); }

bool Platform::IsLive(otInstance* instance) {
  return instance != nullptr && otInstanceIsInitialized(instance);
}

int64_t Platform::NowNanos() const { return async::Now(dispatcher_).get(); }

}  // namespace ot_rcp
