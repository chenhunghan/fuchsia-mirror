// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/flatland_session_types.h"

#include <lib/fit/result.h>
#include <lib/zx/counter.h>
#include <lib/zx/event.h>
#include <lib/zx/handle.h>
#include <zircon/errors.h>
#include <zircon/status.h>
#include <zircon/syscalls/object.h>
#include <zircon/types.h>

#include <utility>
#include <variant>
#include <vector>

namespace flatland {

fit::result<zx_status_t, WaitFence> WaitFence::From(fuchsia_ui_composition::wire::WaitFence fence) {
  WaitFence result;
  switch (fence.Which()) {
    case fuchsia_ui_composition::wire::WaitFence::Tag::kBasic: {
      zx_handle_t raw_handle = fence.basic().get();
      if (raw_handle == ZX_HANDLE_INVALID) {
        return fit::error(ZX_ERR_BAD_HANDLE);
      }
      zx_info_handle_basic_t info;
      zx_status_t status = zx_object_get_info(raw_handle, ZX_INFO_HANDLE_BASIC, &info, sizeof(info),
                                              nullptr, nullptr);
      if (status != ZX_OK) {
        return fit::error(status);
      }
      if (info.type != ZX_OBJ_TYPE_EVENT && info.type != ZX_OBJ_TYPE_EVENTPAIR) {
        return fit::error(ZX_ERR_WRONG_TYPE);
      }
      if (!(info.rights & ZX_RIGHT_WAIT)) {
        return fit::error(ZX_ERR_ACCESS_DENIED);
      }
      result.fence_ = zx::event(fence.basic().release());
      return fit::ok(std::move(result));
    }
    case fuchsia_ui_composition::wire::WaitFence::Tag::kTimestamp: {
      zx_handle_t raw_handle = fence.timestamp().get();
      if (raw_handle == ZX_HANDLE_INVALID) {
        return fit::error(ZX_ERR_BAD_HANDLE);
      }
      zx_info_handle_basic_t info;
      zx_status_t status = zx_object_get_info(raw_handle, ZX_INFO_HANDLE_BASIC, &info, sizeof(info),
                                              nullptr, nullptr);
      if (status != ZX_OK) {
        return fit::error(status);
      }
      if (info.type != ZX_OBJ_TYPE_COUNTER) {
        return fit::error(ZX_ERR_WRONG_TYPE);
      }
      if (!(info.rights & ZX_RIGHT_WAIT)) {
        return fit::error(ZX_ERR_ACCESS_DENIED);
      }
      result.fence_ = std::move(fence.timestamp());
      return fit::ok(std::move(result));
    }
  }
}

bool WaitFence::is_valid() const {
  if (const auto* ev = std::get_if<zx::event>(&fence_)) {
    return ev->is_valid();
  }
  if (const auto* c = std::get_if<zx::counter>(&fence_)) {
    return c->is_valid();
  }
  return false;
}

zx::handle WaitFence::TakeHandle() {
  auto fence = std::exchange(fence_, std::monostate{});
  if (auto* ev = std::get_if<zx::event>(&fence)) {
    return std::move(*ev);
  }
  if (auto* c = std::get_if<zx::counter>(&fence)) {
    return std::move(*c);
  }
  return zx::handle{};
}

fit::result<zx_status_t, SignalFence> SignalFence::From(
    fuchsia_ui_composition::wire::SignalFence fence) {
  SignalFence result;
  switch (fence.Which()) {
    case fuchsia_ui_composition::wire::SignalFence::Tag::kBasic: {
      zx_handle_t raw_handle = fence.basic().get();
      if (raw_handle == ZX_HANDLE_INVALID) {
        return fit::error(ZX_ERR_BAD_HANDLE);
      }
      zx_info_handle_basic_t info;
      zx_status_t status = zx_object_get_info(raw_handle, ZX_INFO_HANDLE_BASIC, &info, sizeof(info),
                                              nullptr, nullptr);
      if (status != ZX_OK) {
        return fit::error(status);
      }
      if (info.type != ZX_OBJ_TYPE_EVENT && info.type != ZX_OBJ_TYPE_EVENTPAIR) {
        return fit::error(ZX_ERR_WRONG_TYPE);
      }
      if (!(info.rights & ZX_RIGHT_SIGNAL)) {
        return fit::error(ZX_ERR_ACCESS_DENIED);
      }
      result.fence_ = zx::event(fence.basic().release());
      return fit::ok(std::move(result));
    }
    case fuchsia_ui_composition::wire::SignalFence::Tag::kTimestamp: {
      zx_handle_t raw_handle = fence.timestamp().get();
      if (raw_handle == ZX_HANDLE_INVALID) {
        return fit::error(ZX_ERR_BAD_HANDLE);
      }
      zx_info_handle_basic_t info;
      zx_status_t status = zx_object_get_info(raw_handle, ZX_INFO_HANDLE_BASIC, &info, sizeof(info),
                                              nullptr, nullptr);
      if (status != ZX_OK) {
        return fit::error(status);
      }
      if (info.type != ZX_OBJ_TYPE_COUNTER) {
        return fit::error(ZX_ERR_WRONG_TYPE);
      }
      constexpr zx_rights_t kRequiredRights = ZX_RIGHT_WRITE | ZX_RIGHT_SIGNAL;
      if ((info.rights & kRequiredRights) != kRequiredRights) {
        return fit::error(ZX_ERR_ACCESS_DENIED);
      }
      result.fence_ = std::move(fence.timestamp());
      return fit::ok(std::move(result));
    }
  }
}

bool SignalFence::is_valid() const {
  if (const auto* ev = std::get_if<zx::event>(&fence_)) {
    return ev->is_valid();
  }
  if (const auto* c = std::get_if<zx::counter>(&fence_)) {
    return c->is_valid();
  }
  return false;
}

void SignalFence::MoveInto(std::vector<zx::event>& events, std::vector<zx::counter>& counters) {
  if (auto* ev = std::get_if<zx::event>(&fence_)) {
    if (ev->is_valid()) {
      events.push_back(std::move(*ev));
    }
    fence_ = std::monostate{};
  } else if (auto* c = std::get_if<zx::counter>(&fence_)) {
    if (c->is_valid()) {
      counters.push_back(std::move(*c));
    }
    fence_ = std::monostate{};
  }
}

}  // namespace flatland
