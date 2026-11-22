// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/platform/radio.h"

#include <endian.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/assert.h>
#include <zircon/status.h>

#include <algorithm>
#include <cstring>
#include <memory>
#include <span>
#include <utility>

#include <openthread/platform/radio.h>

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/platform/platform.h"

namespace ot_rcp {
namespace {

// Referencing IEEE 802.15.4-2015 §7.2.1 Frame Control Field.
constexpr uint8_t kFcfFrameTypeMask = 0x7;
constexpr uint8_t kFcfFrameTypeAck = 0x2;
constexpr uint8_t kFcfAckRequest = 1 << 5;

// Frame control field (2), sequence number (1), and FCS (2): the shortest frame worth looking at.
constexpr uint8_t kMinFrameLength = 5;

}  // namespace

Radio::Radio(async_dispatcher_t* dispatcher, RadioMedium& medium)
    : dispatcher_(dispatcher), medium_(medium) {
  tx_frame_.mPsdu = tx_psdu_;
  tx_task_.set_handler([this] { CompleteTransmit(); });
  medium_.SetReceiver(this);
}

Radio::~Radio() { medium_.SetReceiver(nullptr); }

void Radio::GetIeeeEui64(uint8_t* eui64) const {
  // The EUI-64 is laid out most significant byte first.
  const uint64_t eui64_be = htobe64(Platform::Get().eui64());
  static_assert(sizeof(eui64_be) == OT_EXT_ADDRESS_SIZE);
  memcpy(eui64, &eui64_be, sizeof(eui64_be));
}

otRadioCaps Radio::GetCaps() const { return OT_RADIO_CAPS_NONE; }

otError Radio::Enable() {
  if (config_.state == OT_RADIO_STATE_DISABLED) {
    config_.state = OT_RADIO_STATE_SLEEP;
  }
  return OT_ERROR_NONE;
}

otError Radio::Disable() {
  AbortTransmit();
  config_.state = OT_RADIO_STATE_DISABLED;
  instance_ = nullptr;
  return OT_ERROR_NONE;
}

otError Radio::Sleep() {
  if (config_.state == OT_RADIO_STATE_DISABLED) {
    return OT_ERROR_INVALID_STATE;
  }
  if (config_.state == OT_RADIO_STATE_TRANSMIT) {
    return OT_ERROR_BUSY;
  }
  config_.state = OT_RADIO_STATE_SLEEP;
  return OT_ERROR_NONE;
}

otError Radio::Receive(otInstance* instance, uint8_t channel) {
  ZX_ASSERT(instance != nullptr);
  if (config_.state == OT_RADIO_STATE_DISABLED || config_.state == OT_RADIO_STATE_TRANSMIT) {
    return OT_ERROR_INVALID_STATE;
  }
  instance_ = instance;
  receive_channel_ = channel;
  config_.state = OT_RADIO_STATE_RECEIVE;
  return OT_ERROR_NONE;
}

otError Radio::Transmit(otInstance* instance, otRadioFrame* frame) {
  ZX_ASSERT(instance != nullptr);
  if (config_.state != OT_RADIO_STATE_RECEIVE || frame != &tx_frame_) {
    return OT_ERROR_INVALID_STATE;
  }
  zx_status_t status = tx_task_.Post(dispatcher_);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to post radio transmit task: " << zx_status_get_string(status);
    return OT_ERROR_FAILED;
  }
  config_.state = OT_RADIO_STATE_TRANSMIT;
  tx_instance_ = instance;
  return OT_ERROR_NONE;
}

void Radio::HandleReceivedFrame(const ReceivedFrame& received) {
  const uint8_t channel = received.frame.channel;
  const std::span<const uint8_t> psdu = received.frame.psdu;
  if (config_.state != OT_RADIO_STATE_RECEIVE || channel != receive_channel_) {
    FX_LOGS(DEBUG) << "Not receiving on channel " << static_cast<int>(channel) << ": dropping a "
                   << psdu.size() << " byte frame";
    return;
  }
  if (psdu.size() < kMinFrameLength || psdu.size() > OT_RADIO_FRAME_MAX_SIZE) {
    FX_LOGS(WARNING) << "Dropping a received frame of invalid length " << psdu.size();
    return;
  }
  if ((psdu[0] & kFcfFrameTypeMask) == kFcfFrameTypeAck) {
    // Nothing waits for acknowledgments yet.
    FX_LOGS(DEBUG) << "Dropping a received acknowledgment";
    return;
  }

  ZX_ASSERT(instance_ != nullptr);
  std::ranges::copy(psdu, rx_psdu_);
  // Assigned whole, so that nothing is left over from the previous frame.
  rx_frame_ = {
      .mPsdu = rx_psdu_,
      .mLength = static_cast<uint16_t>(psdu.size()),
      .mChannel = channel,
      .mInfo =
          {
              .mRxInfo =
                  {
                      .mTimestamp = Platform::Get().NowMicros(),
                      .mRssi = received.rssi_dbm,
                      .mLqi = received.lqi,
                  },
          },
  };
  otPlatRadioReceiveDone(instance_, &rx_frame_, OT_ERROR_NONE);
}

otError Radio::GetCoexMetrics(otRadioCoexMetrics* metrics) const {
  if (metrics == nullptr) {
    return OT_ERROR_INVALID_ARGS;
  }
  memset(metrics, 0, sizeof(*metrics));
  return OT_ERROR_NONE;
}

otError Radio::SetCoexEnabled(bool enabled) {
  config_.coex_enabled = enabled;
  return OT_ERROR_NONE;
}

otError Radio::GetTransmitPower(int8_t* power) const {
  if (power == nullptr) {
    return OT_ERROR_INVALID_ARGS;
  }
  *power = config_.tx_power;
  return OT_ERROR_NONE;
}

otError Radio::SetTransmitPower(int8_t power) {
  config_.tx_power = power;
  return OT_ERROR_NONE;
}

otError Radio::GetCcaEnergyDetectThreshold(int8_t* threshold) const {
  if (threshold == nullptr) {
    return OT_ERROR_INVALID_ARGS;
  }
  *threshold = config_.cca_threshold;
  return OT_ERROR_NONE;
}

otError Radio::SetCcaEnergyDetectThreshold(int8_t threshold) {
  config_.cca_threshold = threshold;
  return OT_ERROR_NONE;
}

void Radio::AbortTransmit() {
  tx_task_.Cancel();
  tx_instance_ = nullptr;
  // Expire the token so the medium's pending `done` callback for this transmission, if any, is
  // ignored.
  tx_token_.reset();
}

void Radio::CompleteTransmit() {
  otInstance* instance = tx_instance_;
  otRadioFrame* frame = &tx_frame_;
  if (instance == nullptr || config_.state != OT_RADIO_STATE_TRANSMIT) {
    return;
  }

  otPlatRadioTxStarted(instance, frame);

  tx_token_ = std::make_shared<TxToken>();
  ZX_ASSERT_MSG(frame->mLength <= OT_RADIO_FRAME_MAX_SIZE, "frame length %u exceeds %d",
                frame->mLength, OT_RADIO_FRAME_MAX_SIZE);
  // The medium calls `done` on a later loop iteration, and the completion is reported from
  // OnMediumDone.
  medium_.Transmit({.channel = frame->mChannel, .psdu = {frame->mPsdu, frame->mLength}},
                   config_.tx_power, [this, token = std::weak_ptr<TxToken>(tx_token_)] {
                     // `tx_token_` holds the only strong reference to the token, and everything
                     // runs on one dispatcher. An expired token means the transmission was
                     // aborted or the radio destroyed, so `this` must not be touched.
                     if (std::shared_ptr<TxToken> alive = token.lock()) {
                       OnMediumDone(alive);
                     }
                   });
}

void Radio::OnMediumDone(const std::shared_ptr<TxToken>& token) {
  // A token is replaced only after the previous one was released by completion or abort, so a
  // live token is always the current one.
  ZX_ASSERT(token != nullptr);
  ZX_ASSERT(token == tx_token_);
  tx_token_.reset();

  otInstance* instance = tx_instance_;
  otRadioFrame* frame = &tx_frame_;
  ZX_ASSERT(instance != nullptr);
  ZX_ASSERT_MSG(config_.state == OT_RADIO_STATE_TRANSMIT, "radio state %d",
                static_cast<int>(config_.state));
  const bool ack_requested = frame->mLength > 0 && (frame->mPsdu[0] & kFcfAckRequest) != 0;
  // Move to Receive, on the channel just transmitted on, before reporting completion: OpenThread
  // may start another transmission, or switch channel, from within `otPlatRadioTxDone()`. Staying
  // on the transmit channel is what OpenThread expects, since the acknowledgment comes back on it:
  // the RCP's SubMac switches to the frame's `mRxChannelAfterTxDone` itself, and only when it
  // differs (see `SubMac::HandleTransmitDone()`).
  receive_channel_ = frame->mChannel;
  config_.state = OT_RADIO_STATE_RECEIVE;
  tx_instance_ = nullptr;
  otPlatRadioTxDone(instance, frame, nullptr, ack_requested ? OT_ERROR_NO_ACK : OT_ERROR_NONE);
}

}  // namespace ot_rcp
