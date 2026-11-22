// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Unit tests for the ot-rcp-fuchsia stub radio (radio.cc).
//
// The OpenThread core is not linked: the radio callbacks into the core (otPlatRadioTxStarted,
// otPlatRadioTxDone, otPlatRadioReceiveDone) are faked.

#include <lib/async-testing/test_loop.h>
#include <lib/async/cpp/task.h>
#include <lib/fit/function.h>
#include <zircon/assert.h>

#include <cstdint>
#include <cstring>
#include <optional>
#include <utility>
#include <vector>

#include <gtest/gtest.h>
#include <openthread/platform/radio.h>

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/platform/platform.h"

namespace {

// Fake OpenThread implementation.

// A frame the radio reported with otPlatRadioReceiveDone(), copied out of the radio's buffer.
struct ReportedFrame {
  std::vector<uint8_t> psdu;
  uint8_t channel;
  int8_t rssi;
  uint8_t lqi;
  uint64_t timestamp;

  bool operator==(const ReportedFrame&) const = default;
};

// Records the radio's calls into the (faked) OpenThread SubMac.
struct FakeSubMac {
  int tx_started_calls = 0;
  otRadioFrame* last_started_frame = nullptr;
  int tx_done_calls = 0;
  otRadioFrame* last_done_frame = nullptr;
  otRadioFrame* last_done_ack = nullptr;
  otError last_done_error = OT_ERROR_NONE;
  // Optional action run from within otPlatRadioTxDone(), to exercise re-entry into the radio the
  // way the SubMac does.
  fit::closure on_tx_done;
  std::vector<ReportedFrame> received;
};

FakeSubMac& SubMac() {
  static FakeSubMac sub_mac;
  return sub_mac;
}

}  // namespace

extern "C" {

void otPlatRadioTxStarted(otInstance* aInstance, otRadioFrame* aFrame) {
  EXPECT_NE(aInstance, nullptr);
  FakeSubMac& sub_mac = SubMac();
  sub_mac.tx_started_calls++;
  sub_mac.last_started_frame = aFrame;
  // TxStarted always precedes TxDone for the same transmission.
  EXPECT_EQ(sub_mac.tx_started_calls, sub_mac.tx_done_calls + 1);
}

void otPlatRadioTxDone(otInstance* aInstance, otRadioFrame* aFrame, otRadioFrame* aAckFrame,
                       otError aError) {
  EXPECT_NE(aInstance, nullptr);
  FakeSubMac& sub_mac = SubMac();
  sub_mac.tx_done_calls++;
  sub_mac.last_done_frame = aFrame;
  sub_mac.last_done_ack = aAckFrame;
  sub_mac.last_done_error = aError;
  if (sub_mac.on_tx_done) {
    sub_mac.on_tx_done();
  }
}

void otPlatRadioReceiveDone(otInstance* aInstance, otRadioFrame* aFrame, otError aError) {
  EXPECT_NE(aInstance, nullptr);
  EXPECT_EQ(aError, OT_ERROR_NONE);
  ASSERT_NE(aFrame, nullptr);
  SubMac().received.push_back({
      .psdu = std::vector<uint8_t>(aFrame->mPsdu, aFrame->mPsdu + aFrame->mLength),
      .channel = aFrame->mChannel,
      .rssi = aFrame->mInfo.mRxInfo.mRssi,
      .lqi = aFrame->mInfo.mRxInfo.mLqi,
      .timestamp = aFrame->mInfo.mRxInfo.mTimestamp,
  });
}

}  // extern "C"

namespace ot_rcp {
namespace {

constexpr uint32_t kNodeId = 7;
constexpr uint8_t kChannel = 15;

// Referencing IEEE 802.15.4-2015 §7.2.1 Frame Control Field.
constexpr uint8_t kFcfTypeData = 0x1;
constexpr uint8_t kFcfAckRequest = 1 << 5;
constexpr uint8_t kFcfPanIdCompression = 1 << 6;
constexpr uint8_t kFcfDstAddrShort = 0b10 << 2;
constexpr uint8_t kFcfVersion2003 = 0b00 << 4;
constexpr uint8_t kFcfSrcAddrShort = 0b10 << 6;

// Length of a minimal data frame: 9-byte MHR (FCF 2, sequence 1, destination PAN ID 2,
// short destination 2, short source 2) + 5-byte payload + 2-byte FCS.
constexpr uint16_t kFrameLength = 16;

// `otInstance` is only passed by platform code.
otInstance* FakeInstance() {
  static uint64_t storage;
  return reinterpret_cast<otInstance*>(&storage);
}

// A fake RadioMedium for tests that do not care about the medium.
//
// Like the real medium (RfsimClient), whose `done` runs from a FIDL reply, this fake never calls
// `done` before `Transmit` returns: it posts `done` to the dispatcher, so the medium replies on the
// next loop iteration and the test never has to drive it by hand.
//
// It never delivers frames to the radio: `SetReceiver` ignores the receiver.
class AutoReplyMedium : public RadioMedium {
 public:
  explicit AutoReplyMedium(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {}

  void Transmit(const MediumFrame& frame, int8_t tx_power_dbm, DoneFn done) override {
    ASSERT_EQ(async::PostTask(dispatcher_, [done = std::move(done)]() mutable { done(); }), ZX_OK);
  }
  void SetReceiver(RadioReceiver* receiver) override {}

 private:
  async_dispatcher_t* const dispatcher_;
};

class RadioTest : public ::testing::Test {
 protected:
  void SetUp() override {
    SubMac() = FakeSubMac();
    platform_.emplace(loop_.dispatcher(), kNodeId, auto_reply_medium_);
  }

  void TearDown() override { platform_.reset(); }

  // Brings the radio to the Receive state
  void EnableAndReceive() {
    ASSERT_EQ(otPlatRadioEnable(FakeInstance()), OT_ERROR_NONE);
    ASSERT_EQ(otPlatRadioReceive(FakeInstance(), kChannel), OT_ERROR_NONE);
    ASSERT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_RECEIVE);
  }

  // Fills the radio's transmit buffer with a minimal data frame (short addresses, PAN ID
  // compression) and returns it.
  otRadioFrame* PrepareFrame(bool ack_request) {
    otRadioFrame* frame = otPlatRadioGetTransmitBuffer(FakeInstance());
    frame->mChannel = kChannel;
    frame->mLength = kFrameLength;
    frame->mPsdu[0] = kFcfTypeData | kFcfPanIdCompression | (ack_request ? kFcfAckRequest : 0);
    frame->mPsdu[1] = kFcfDstAddrShort | kFcfVersion2003 | kFcfSrcAddrShort;
    return frame;
  }

  async::TestLoop loop_;
  AutoReplyMedium auto_reply_medium_{loop_.dispatcher()};
  // Declared after `loop_` and `auto_reply_medium_` so that it is destroyed first, even if
  // TearDown() does not run.
  std::optional<Platform> platform_;
};

// State machine

TEST_F(RadioTest, StartsDisabledAndEnableMovesToSleep) {
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_DISABLED);
  EXPECT_FALSE(otPlatRadioIsEnabled(FakeInstance()));

  EXPECT_EQ(otPlatRadioEnable(FakeInstance()), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_SLEEP);
  EXPECT_TRUE(otPlatRadioIsEnabled(FakeInstance()));

  // Enabling an enabled radio keeps its current state.
  EXPECT_EQ(otPlatRadioReceive(FakeInstance(), kChannel), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioEnable(FakeInstance()), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_RECEIVE);
}

TEST_F(RadioTest, ReceiveAndSleepRequireEnabledRadio) {
  EXPECT_EQ(otPlatRadioReceive(FakeInstance(), kChannel), OT_ERROR_INVALID_STATE);
  EXPECT_EQ(otPlatRadioSleep(FakeInstance()), OT_ERROR_INVALID_STATE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_DISABLED);
}

TEST_F(RadioTest, SleepAndReceiveAlternate) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  EXPECT_EQ(otPlatRadioSleep(FakeInstance()), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_SLEEP);
  EXPECT_EQ(otPlatRadioReceive(FakeInstance(), kChannel), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_RECEIVE);
}

// Disable from Transmit is covered by DisableDuringTransmitDropsCompletion.
TEST_F(RadioTest, DisableReturnsToDisabledFromSleepAndReceive) {
  ASSERT_EQ(otPlatRadioEnable(FakeInstance()), OT_ERROR_NONE);
  ASSERT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_SLEEP);
  EXPECT_EQ(otPlatRadioDisable(FakeInstance()), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_DISABLED);
  EXPECT_FALSE(otPlatRadioIsEnabled(FakeInstance()));

  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  EXPECT_EQ(otPlatRadioDisable(FakeInstance()), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_DISABLED);
  EXPECT_FALSE(otPlatRadioIsEnabled(FakeInstance()));
}

// Transmit

TEST_F(RadioTest, TransmitRequiresReceiveState) {
  otRadioFrame* frame = PrepareFrame(/*ack_request=*/false);

  EXPECT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_INVALID_STATE);  // Disabled.
  ASSERT_EQ(otPlatRadioEnable(FakeInstance()), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_INVALID_STATE);  // Sleep.

  loop_.RunUntilIdle();
  EXPECT_EQ(SubMac().tx_started_calls, 0);
  EXPECT_EQ(SubMac().tx_done_calls, 0);
}

// OpenThread always transmits the frame obtained from otPlatRadioGetTransmitBuffer().
TEST_F(RadioTest, TransmitRejectsForeignFrame) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  uint8_t psdu[OT_RADIO_FRAME_MAX_SIZE] = {};
  otRadioFrame foreign = {};
  foreign.mPsdu = psdu;
  foreign.mLength = kFrameLength;
  foreign.mChannel = kChannel;

  EXPECT_EQ(otPlatRadioTransmit(FakeInstance(), &foreign), OT_ERROR_INVALID_STATE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_RECEIVE);
}

TEST_F(RadioTest, TransmitOneFrame) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  otRadioFrame* frame = PrepareFrame(/*ack_request=*/false);

  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_TRANSMIT);
  EXPECT_EQ(SubMac().tx_started_calls, 0);
  EXPECT_EQ(SubMac().tx_done_calls, 0);

  loop_.RunUntilIdle();
  EXPECT_EQ(SubMac().tx_started_calls, 1);
  EXPECT_EQ(SubMac().last_started_frame, frame);
  EXPECT_EQ(SubMac().tx_done_calls, 1);
  EXPECT_EQ(SubMac().last_done_frame, frame);
  EXPECT_EQ(SubMac().last_done_ack, nullptr);
  EXPECT_EQ(SubMac().last_done_error, OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_RECEIVE);
}

TEST_F(RadioTest, TransmitWhileTransmittingIsRejected) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  otRadioFrame* frame = PrepareFrame(/*ack_request=*/false);

  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_INVALID_STATE);

  loop_.RunUntilIdle();
  EXPECT_EQ(SubMac().tx_started_calls, 1);
  EXPECT_EQ(SubMac().tx_done_calls, 1);
}

TEST_F(RadioTest, DisableDuringTransmitDropsCompletion) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  otRadioFrame* frame = PrepareFrame(/*ack_request=*/false);
  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);

  EXPECT_EQ(otPlatRadioDisable(FakeInstance()), OT_ERROR_NONE);
  loop_.RunUntilIdle();
  EXPECT_EQ(SubMac().tx_started_calls, 0);
  EXPECT_EQ(SubMac().tx_done_calls, 0);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_DISABLED);

  // The radio is usable again afterwards.
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);
  loop_.RunUntilIdle();
  EXPECT_EQ(SubMac().tx_done_calls, 1);
}

TEST_F(RadioTest, TransmitFromWithinTxDoneIsAccepted) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  otRadioFrame* frame = PrepareFrame(/*ack_request=*/true);
  SubMac().on_tx_done = [frame] {
    if (SubMac().tx_done_calls == 1) {
      EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_RECEIVE);
      EXPECT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);
    }
  };

  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);
  loop_.RunUntilIdle();
  EXPECT_EQ(SubMac().tx_started_calls, 2);
  EXPECT_EQ(SubMac().tx_done_calls, 2);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_RECEIVE);
}

// Interaction with the RadioMedium

// A fake RadioMedium that lets the test decide when the medium replies.
//
// `Transmit` does not call `done`: it keeps ("holds") it until the test calls `Release()`, which
// stands in for the medium's reply arriving. In between, the radio is where it would be while a
// real medium is still busy with the frame: handed over, but not acknowledged yet. A test can act
// in that window (disable the radio, destroy it, ...) and then check what the late reply does.
//
// It also counts the frames it is handed, and fails the test if the radio hands over a second
// frame before the first one's `done` has run: the radio must transmit one frame at a time.
//
// What is handed over (channel, PSDU, power) is checked end to end by
// spinel_device_server_unittest.cc; the tests here cover the timing, which cannot be controlled
// from outside.
//
// It also remembers the receiver the radio registers, so a test can deliver frames to the radio
// as the medium would (see `receiver()`).
class HoldingMedium : public RadioMedium {
 public:
  void Transmit(const MediumFrame& frame, int8_t tx_power_dbm, DoneFn done) override {
    transmissions_++;
    EXPECT_FALSE(pending_done_) << "radio transmitted again before the previous done";
    pending_done_ = std::move(done);
  }
  void SetReceiver(RadioReceiver* receiver) override { receiver_ = receiver; }

  // How many frames the radio has handed over.
  int transmissions() const { return transmissions_; }
  // Whether a frame's `done` is held, i.e. the medium has not replied yet.
  bool holding() const { return static_cast<bool>(pending_done_); }
  // The radio currently registered for received frames, or null.
  RadioReceiver* receiver() const { return receiver_; }

  // Runs the held `done`, as the medium's reply would.
  void Release() {
    ASSERT_TRUE(pending_done_) << "nothing to release";
    DoneFn done = std::move(pending_done_);
    done();
  }

 private:
  int transmissions_ = 0;
  DoneFn pending_done_;
  RadioReceiver* receiver_ = nullptr;
};

class RadioMediumTest : public RadioTest {
 protected:
  void SetUp() override {
    SubMac() = FakeSubMac();
    platform_.emplace(loop_.dispatcher(), kNodeId, medium_);
  }

  // Delivers `psdu` to the radio as the medium would.
  void Deliver(uint8_t channel, const std::vector<uint8_t>& psdu, int8_t rssi_dbm = -60,
               uint8_t lqi = 255) {
    ZX_ASSERT(medium_.receiver() != nullptr);
    medium_.receiver()->HandleReceivedFrame({
        .frame = {.channel = channel, .psdu = psdu},
        .rssi_dbm = rssi_dbm,
        .lqi = lqi,
    });
  }

  // A minimal data frame (see PrepareFrame) with sequence number `seq` and a zero FCS.
  static std::vector<uint8_t> DataFrame(uint8_t seq) {
    std::vector<uint8_t> psdu(kFrameLength, 0);
    psdu[0] = kFcfTypeData | kFcfPanIdCompression;
    psdu[1] = kFcfDstAddrShort | kFcfVersion2003 | kFcfSrcAddrShort;
    psdu[2] = seq;
    return psdu;
  }

  // Outlives `platform_`, which TearDown() resets.
  HoldingMedium medium_;
};

// TxDone is the medium's reply: it does not come before the medium has run `done`, and the radio
// stays in Transmit (refusing another frame) until then.
TEST_F(RadioMediumTest, TxDoneWaitsForMedium) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  otRadioFrame* frame = PrepareFrame(/*ack_request=*/false);
  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);
  loop_.RunUntilIdle();

  EXPECT_EQ(medium_.transmissions(), 1);
  EXPECT_TRUE(medium_.holding());
  EXPECT_EQ(SubMac().tx_started_calls, 1);
  EXPECT_EQ(SubMac().tx_done_calls, 0);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_TRANSMIT);
  EXPECT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_INVALID_STATE);

  ASSERT_NO_FATAL_FAILURE(medium_.Release());
  EXPECT_EQ(SubMac().tx_done_calls, 1);
  EXPECT_EQ(SubMac().last_done_frame, frame);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_RECEIVE);

  // And the next frame goes out.
  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);
  loop_.RunUntilIdle();
  EXPECT_EQ(medium_.transmissions(), 2);
  ASSERT_NO_FATAL_FAILURE(medium_.Release());
  EXPECT_EQ(SubMac().tx_done_calls, 2);
}

// Disable() during a transmission drops it, whether the transmit task has not run yet or the
// medium already holds the frame: the medium does not see the frame in the first case, the late
// `done` reports no TxDone in the second, and neither disturbs the transmission after it.
TEST_F(RadioMediumTest, DisableDuringTransmitDropsTheFrameAndItsCompletion) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  otRadioFrame* frame = PrepareFrame(/*ack_request=*/false);

  // Disabled before the transmit task runs: the frame never reaches the medium.
  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);
  ASSERT_EQ(otPlatRadioDisable(FakeInstance()), OT_ERROR_NONE);
  loop_.RunUntilIdle();
  EXPECT_EQ(medium_.transmissions(), 0);
  EXPECT_EQ(SubMac().tx_done_calls, 0);

  // Disabled while the medium holds the frame: its late `done` reports nothing.
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);
  loop_.RunUntilIdle();
  EXPECT_EQ(medium_.transmissions(), 1);
  ASSERT_TRUE(medium_.holding());
  ASSERT_EQ(otPlatRadioDisable(FakeInstance()), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_DISABLED);
  ASSERT_NO_FATAL_FAILURE(medium_.Release());  // The medium replies to the abandoned frame.
  EXPECT_EQ(SubMac().tx_done_calls, 0);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_DISABLED);

  // A new transmission has its own completion. The abandoned one had a TxStarted and, correctly,
  // no TxDone; reset the fake so its started/done pairing check applies to the new one only.
  SubMac() = FakeSubMac();
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);
  loop_.RunUntilIdle();
  EXPECT_EQ(medium_.transmissions(), 2);
  ASSERT_NO_FATAL_FAILURE(medium_.Release());
  EXPECT_EQ(SubMac().tx_done_calls, 1);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_RECEIVE);
}

// The medium's `done` arriving after the radio is gone (the platform is re-created on every reset)
// must be harmless.
TEST_F(RadioMediumTest, MediumDoneAfterRadioDestroyedIsIgnored) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  otRadioFrame* frame = PrepareFrame(/*ack_request=*/false);
  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);
  loop_.RunUntilIdle();
  ASSERT_TRUE(medium_.holding());

  platform_.reset();
  ASSERT_NO_FATAL_FAILURE(medium_.Release());
  EXPECT_EQ(SubMac().tx_done_calls, 0);
}

// After a transmission the radio keeps receiving on the frame's channel, whatever channel it
// received on before.
TEST_F(RadioMediumTest, ReceivesOnTransmitChannelAfterTxDone) {
  ASSERT_EQ(otPlatRadioEnable(FakeInstance()), OT_ERROR_NONE);
  ASSERT_EQ(otPlatRadioReceive(FakeInstance(), kChannel + 1), OT_ERROR_NONE);

  otRadioFrame* frame = PrepareFrame(/*ack_request=*/false);
  ASSERT_EQ(frame->mChannel, kChannel);
  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);
  loop_.RunUntilIdle();
  ASSERT_NO_FATAL_FAILURE(medium_.Release());
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_RECEIVE);

  Deliver(kChannel + 1, DataFrame(/*seq=*/1));
  EXPECT_TRUE(SubMac().received.empty());
  Deliver(kChannel, DataFrame(/*seq=*/2));
  EXPECT_EQ(SubMac().received.size(), 1u);
}

// OpenThread's SubMac may switch the receive channel from within otPlatRadioTxDone(); TxDone must
// not overwrite that switch with the transmitted frame's channel.
TEST_F(RadioMediumTest, ReceiveFromWithinTxDoneSwitchesChannel) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  SubMac().on_tx_done = [] {
    EXPECT_EQ(otPlatRadioReceive(FakeInstance(), kChannel + 2), OT_ERROR_NONE);
  };

  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), PrepareFrame(/*ack_request=*/false)),
            OT_ERROR_NONE);
  loop_.RunUntilIdle();
  ASSERT_NO_FATAL_FAILURE(medium_.Release());

  Deliver(kChannel, DataFrame(/*seq=*/1));
  EXPECT_TRUE(SubMac().received.empty());
  Deliver(kChannel + 2, DataFrame(/*seq=*/2));
  EXPECT_EQ(SubMac().received.size(), 1u);
}

// The radio is the medium's receiver for as long as it exists.
TEST_F(RadioMediumTest, RadioIsReceiverWhileItExists) {
  EXPECT_EQ(medium_.receiver(), &Platform::Get().radio());
  platform_.reset();
  EXPECT_EQ(medium_.receiver(), nullptr);
}

TEST_F(RadioMediumTest, ReceivedFrameIsReportedWhileReceivingOnItsChannel) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  loop_.AdvanceTimeByEpsilon();
  const uint64_t now = Platform::Get().NowMicros();
  const std::vector<uint8_t> psdu = DataFrame(/*seq=*/7);
  Deliver(kChannel, psdu, /*rssi_dbm=*/-42, /*lqi=*/200);

  EXPECT_EQ(SubMac().received, (std::vector<ReportedFrame>{{
                                   .psdu = psdu,
                                   .channel = kChannel,
                                   .rssi = -42,
                                   .lqi = 200,
                                   .timestamp = now,
                               }}));
}

TEST_F(RadioMediumTest, ReceivedFrameIsDroppedUnlessReceivingOnItsChannel) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  const std::vector<uint8_t> psdu = DataFrame(/*seq=*/7);

  Deliver(kChannel + 1, psdu);  // Another channel.

  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), PrepareFrame(/*ack_request=*/false)),
            OT_ERROR_NONE);
  Deliver(kChannel, psdu);  // Transmitting on kChannel.
  loop_.RunUntilIdle();
  ASSERT_NO_FATAL_FAILURE(medium_.Release());

  ASSERT_EQ(otPlatRadioSleep(FakeInstance()), OT_ERROR_NONE);
  Deliver(kChannel, psdu);  // Asleep with receive_channel_ == kChannel.

  ASSERT_EQ(otPlatRadioDisable(FakeInstance()), OT_ERROR_NONE);
  Deliver(kChannel, psdu);  // Disabled with receive_channel_ == kChannel.

  EXPECT_TRUE(SubMac().received.empty());
}

TEST_F(RadioMediumTest, ReceivedFrameIsFilteredByTypeAndLength) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  // Dropped: Imm-Ack (frame type 2), too short (< 5 bytes), and oversized (> 127 bytes).
  Deliver(kChannel, {0x02, 0x00, 0x07, 0x00, 0x00});
  Deliver(kChannel, {});
  Deliver(kChannel, {0x41, 0x88, 0x01, 0x00});
  Deliver(kChannel, std::vector<uint8_t>(OT_RADIO_FRAME_MAX_SIZE + 1, kFcfTypeData));
  EXPECT_TRUE(SubMac().received.empty());

  // Accepted: shortest (5 bytes: FCF 2, sequence 1, FCS 2) and longest (127 bytes) PSDUs.
  const std::vector<uint8_t> min_psdu = {kFcfTypeData, 0x00, 0x09, 0x00, 0x00};
  std::vector<uint8_t> max_psdu = DataFrame(/*seq=*/10);
  max_psdu.resize(OT_RADIO_FRAME_MAX_SIZE, 0xab);
  Deliver(kChannel, min_psdu);
  Deliver(kChannel, max_psdu);
  ASSERT_EQ(SubMac().received.size(), 2u);
  EXPECT_EQ(SubMac().received[0].psdu, min_psdu);
  EXPECT_EQ(SubMac().received[1].psdu, max_psdu);
}

}  // namespace
}  // namespace ot_rcp
