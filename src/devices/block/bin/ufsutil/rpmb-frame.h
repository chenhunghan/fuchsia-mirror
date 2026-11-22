// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_BIN_UFSUTIL_RPMB_FRAME_H_
#define SRC_DEVICES_BLOCK_BIN_UFSUTIL_RPMB_FRAME_H_

#include <fidl/fuchsia.hardware.rpmb/cpp/wire.h>

#include <cstddef>
#include <cstdint>
#include <string_view>

namespace ufsutil {

// RPMB message frame. UFS uses the eMMC layout. Multi-byte fields are big-endian.
struct RpmbFrame {
  uint8_t stuff[196];
  uint8_t key_mac[32];
  uint8_t data[256];
  uint8_t nonce[16];
  uint32_t write_counter;
  uint16_t address;
  uint16_t block_count;
  uint16_t result;
  uint16_t req_resp;
};
static_assert(sizeof(RpmbFrame) == fuchsia_hardware_rpmb::wire::kFrameSize);

constexpr uint16_t kRpmbWriteCounterReadRequest = 0x0002;
constexpr uint16_t kRpmbAuthenticatedDataWriteRequest = 0x0003;
constexpr uint16_t kRpmbAuthenticatedDataReadRequest = 0x0004;
constexpr uint16_t kRpmbResultReadRequest = 0x0005;
constexpr uint16_t kRpmbWriteCounterReadResponse = 0x0200;
constexpr uint16_t kRpmbAuthenticatedDataWriteResponse = 0x0300;
constexpr uint16_t kRpmbAuthenticatedDataReadResponse = 0x0400;

// The authentication key and the MAC are an HMAC-SHA256 key and digest of this size.
constexpr size_t kRpmbKeySize = sizeof(RpmbFrame::key_mac);

// The MAC covers each frame from `data` to the end.
constexpr size_t kRpmbMacOffset = offsetof(RpmbFrame, data);

// Operation results carried in the low bits of `RpmbFrame::result`.
enum class RpmbResult : uint16_t {
  kOk = 0x0000,
  kGeneralFailure = 0x0001,
  kAuthenticationFailure = 0x0002,
  kCounterFailure = 0x0003,
  kAddressFailure = 0x0004,
  kWriteFailure = 0x0005,
  kReadFailure = 0x0006,
  kKeyNotProgrammed = 0x0007,
};

// Set in `RpmbFrame::result` once the write counter has reached its maximum value.
constexpr uint16_t kRpmbResultWriteCounterExpired = 0x0080;

// Returns the operation result of a host-order `RpmbFrame::result` value.
constexpr uint16_t RpmbOperationResult(uint16_t result) {
  return result & static_cast<uint16_t>(~kRpmbResultWriteCounterExpired);
}

constexpr std::string_view RpmbResultString(uint16_t operation_result) {
  switch (static_cast<RpmbResult>(operation_result)) {
    case RpmbResult::kOk:
      return "success";
    case RpmbResult::kGeneralFailure:
      return "general failure";
    case RpmbResult::kAuthenticationFailure:
      return "authentication failure";
    case RpmbResult::kCounterFailure:
      return "counter failure";
    case RpmbResult::kAddressFailure:
      return "address failure";
    case RpmbResult::kWriteFailure:
      return "write failure";
    case RpmbResult::kReadFailure:
      return "read failure";
    case RpmbResult::kKeyNotProgrammed:
      return "authentication key not yet programmed";
  }
  return "unknown result";
}

}  // namespace ufsutil

#endif  // SRC_DEVICES_BLOCK_BIN_UFSUTIL_RPMB_FRAME_H_
