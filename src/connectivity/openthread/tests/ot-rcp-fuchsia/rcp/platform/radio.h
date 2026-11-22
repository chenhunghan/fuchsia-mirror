// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_PLATFORM_RADIO_H_
#define SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_PLATFORM_RADIO_H_

#include <lib/async/cpp/task.h>
#include <lib/async/dispatcher.h>
#include <lib/fit/function.h>

#include <cstdint>
#include <memory>
#include <span>

#include <openthread/platform/radio.h>

namespace ot_rcp {

// A frame on the simulated medium.
struct MediumFrame {
  uint8_t channel;
  // The PSDU, including the Frame Check Sequence. Only valid during the call it is passed to.
  std::span<const uint8_t> psdu;
};

// A frame the simulated medium delivered to this node.
struct ReceivedFrame {
  MediumFrame frame;
  // What this node observes of the frame.
  int8_t rssi_dbm;
  uint8_t lqi;
};

// Receives the frames the simulated medium delivers to this node.
class RadioReceiver {
 public:
  virtual ~RadioReceiver() = default;

  virtual void HandleReceivedFrame(const ReceivedFrame& received) = 0;
};

// The simulated medium as the radio sees it: where its frames go and where received frames come
// from.
//
// Limitation: the radio does not report its state or channel to the medium, so the medium
// delivers every frame to every Thread node attached to it, and the radio drops the frames it would
// not hear.
class RadioMedium {
 public:
  // Invoked exactly once, when the medium has taken the frame. Never runs before `Transmit`
  // returns.
  using DoneFn = fit::callback<void()>;

  virtual ~RadioMedium() = default;

  // `tx_power_dbm` is the radio's configured transmit power. The radio transmits one frame at a
  // time: it does not call this again before `done` has run.
  virtual void Transmit(const MediumFrame& frame, int8_t tx_power_dbm, DoneFn done) = 0;

  // Sets who gets the frames delivered to this node from now on. Frames are delivered only while
  // a receiver is set; it may be null only while the radio is being replaced, which must happen
  // without returning to the loop in between.
  virtual void SetReceiver(RadioReceiver* receiver) = 0;
};

// Stub IEEE 802.15.4 radio for ot-rcp-fuchsia.
//
// The radio keeps a Disabled/Sleep/Receive/Transmit state machine. Transmitted frames are handed to
// the RadioMedium, and TxDone is reported once the medium says the frame is out. Frames the medium
// delivers reach OpenThread only while the radio is in Receive on their channel; acknowledgments
// are dropped, since nothing waits for them yet. No hardware capabilities are reported.
//
// One instance is owned by the Platform (see platform.h) and is reached through
// `Platform::Get().radio()`; the otPlatRadio* entry points in ot_radio_platform.cc forward to it.
// Destroying it cancels an in-flight transmission; the power-on configuration is restored simply
// by constructing a new one.
class Radio : public RadioReceiver {
 public:
  // Settings the host changes over Spinel (PROP_PHY_TX_POWER, PROP_PHY_CCA_THRESHOLD, ...), with
  // their power-on defaults. The defaults match OpenThread's simulation platform.
  struct Config {
    otRadioState state = OT_RADIO_STATE_DISABLED;
    bool promiscuous = false;
    bool coex_enabled = false;
    int8_t tx_power = 0;         // dBm
    int8_t cca_threshold = -75;  // dBm
  };

  // Simulated radio environment.
  struct Environment {
    // Reported by otPlatRadioGetRssi(): an idle channel everywhere.
    int8_t rssi = -100;  // dBm
    // Reported through SPINEL_PROP_PHY_RX_SENSITIVITY; the host uses it as the noise floor when
    // computing link margins.
    int8_t receive_sensitivity = -100;  // dBm
  };

  // `medium` carries what the radio transmits and receives, and must outlive the radio. It is
  // borrowed, since the radio is re-created on every reset, while the node stays attached to the
  // medium. The radio is `medium`'s receiver until it is destroyed.
  Radio(async_dispatcher_t* dispatcher, RadioMedium& medium);
  ~Radio() override;

  Radio(const Radio&) = delete;
  Radio& operator=(const Radio&) = delete;

  // Identity and capabilities.
  void GetIeeeEui64(uint8_t* eui64) const;  // OT_EXT_ADDRESS_SIZE bytes, MSB first.
  otRadioCaps GetCaps() const;

  // State machine.
  otRadioState GetState() const { return config_.state; }
  bool IsEnabled() const { return config_.state != OT_RADIO_STATE_DISABLED; }
  otError Enable();
  // Drops an in-flight transmission without reporting completion, whether it is still waiting for
  // the transmit task or for the medium.
  otError Disable();
  otError Sleep();
  // Moves to Receive on `channel`, reporting received frames to `instance` until disabled or
  // switched again. After a transmission the radio receives on the channel it transmitted on.
  otError Receive(otInstance* instance, uint8_t channel);

  // Transmission.
  otRadioFrame* GetTransmitBuffer() { return &tx_frame_; }
  otError Transmit(otInstance* instance, otRadioFrame* frame);

  // RadioReceiver. Reports the frame with otPlatRadioReceiveDone() if the radio is in Receive on
  // the frame's channel and the frame is not an acknowledgment; drops it otherwise.
  void HandleReceivedFrame(const ReceivedFrame& received) override;

  // Environment.
  int8_t GetRssi() const { return environment_.rssi; }
  int8_t GetReceiveSensitivity() const { return environment_.receive_sensitivity; }
  otError GetCoexMetrics(otRadioCoexMetrics* metrics) const;

  // Host-controlled settings.
  bool GetPromiscuous() const { return config_.promiscuous; }
  void SetPromiscuous(bool enable) { config_.promiscuous = enable; }
  bool IsCoexEnabled() const { return config_.coex_enabled; }
  otError SetCoexEnabled(bool enabled);
  otError GetTransmitPower(int8_t* power) const;
  otError SetTransmitPower(int8_t power);
  otError GetCcaEnergyDetectThreshold(int8_t* threshold) const;
  otError SetCcaEnergyDetectThreshold(int8_t threshold);

  const Config& config() const { return config_; }
  const Environment& environment() const { return environment_; }
  void set_environment(const Environment& environment) { environment_ = environment; }

 private:
  // Identifies one transmission. `tx_token_` is replaced for every transmission and dropped when
  // one is aborted or the radio is destroyed, so a medium's late `done` for it is ignored.
  struct TxToken {};

  void AbortTransmit();
  void CompleteTransmit();
  // Reports TxDone for the in-flight transmission. `token` must be the live `tx_token_`; callers
  // holding only a weak reference lock it first, and skip the call if it expired.
  void OnMediumDone(const std::shared_ptr<TxToken>& token);

  async_dispatcher_t* const dispatcher_;
  RadioMedium& medium_;
  Config config_;
  Environment environment_;
  uint8_t receive_channel_ = 0;
  // Instance received frames are reported to; set by Receive() and cleared by Disable().
  otInstance* instance_ = nullptr;
  otRadioFrame rx_frame_ = {};
  uint8_t rx_psdu_[OT_RADIO_FRAME_MAX_SIZE] = {};
  otRadioFrame tx_frame_ = {};
  uint8_t tx_psdu_[OT_RADIO_FRAME_MAX_SIZE] = {};
  // Instance to report completion to; non-null only while a transmission is in flight.
  otInstance* tx_instance_ = nullptr;
  std::shared_ptr<TxToken> tx_token_;
  async::TaskClosure tx_task_;
};

}  // namespace ot_rcp

#endif  // SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_PLATFORM_RADIO_H_
