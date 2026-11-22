// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Unit test for `SpinelDeviceServer`: the `fuchsia.lowpan.spinel` contract as seen by the host.
//
// The test plays the role of the Spinel FIDL client: it binds `fuchsia.lowpan.spinel.Device`
// through `DeviceSetup`, opens the device, and drives the Spinel handshake the host performs
// during RCP initialization. It also covers the transport semantics (flow control credits, frame
// size limits, error handling and rebinding) and a few raw transmits against the radio.
//
// The RCP's radio transmits into a `test.lowpan.rfsim.Medium`: `FakeMedium` below, served on the
// same loop as the RCP, which records every frame. The medium's own behaviour is tested by
// rf-medium-dispatcher-unittests.

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/spinel_device_server.h"

#include <fidl/fuchsia.lowpan.spinel/cpp/fidl.h>
#include <fidl/test.lowpan.rfsim/cpp/fidl.h>
#include <lib/fidl/cpp/client.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <algorithm>
#include <cstdint>
#include <cstring>
#include <ios>
#include <memory>
#include <optional>
#include <string>
#include <utility>
#include <vector>

#include <gtest/gtest.h>
#include <openthread/platform/radio.h>

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/platform/platform.h"
#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/rfsim_client.h"
#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/tests/mock-spinel-host/mac_frame.h"
#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/tests/mock-spinel-host/mock_spinel_host.h"
#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/tests/mock-spinel-host/mock_spinel_host_fixture.h"
#include "src/lib/testing/predicates/status.h"

namespace ot_rcp::testing {
namespace {

using ot_rcp::Platform;
using ot_rcp::RfsimClient;
using ot_rcp::SpinelDeviceServer;

namespace rfsim = test_lowpan_rfsim;

// Node id the server is created with. Non-trivial, to prove the EUI-64 is derived from it.
constexpr uint32_t kNodeId = 7;

// Every reset the RCP performs (Open, Close, RESET(STACK)) is reported as a power-on reset.
constexpr spinel_status_t kExpectedResetStatus = SPINEL_STATUS_RESET_POWER_ON;

// Transport contract, taken from the server's header. The server is constructed by the test, so
// the flow-control parameters are whatever the test passes in; the defaults match the component.
constexpr uint32_t kExpectedMaxFrameSize = SpinelDeviceServer::kMaxFrameSize;
constexpr SpinelDeviceServer::FlowControl kFlowControl{};
constexpr uint32_t kInitialRadioboundCredit = kFlowControl.radiobound_credit_init;
constexpr uint32_t kRadioboundCreditIncrement = kFlowControl.radiobound_credit_inc;
static_assert(kRadioboundCreditIncrement > 0);
static_assert(
    kInitialRadioboundCredit >= kRadioboundCreditIncrement,
    "the back-pressure tests below assume at least one re-grant fits in the initial window");
// Credit re-granted once the whole initial window has been consumed: one increment per full
// increment of frames (integer division: a partial increment earns nothing).
constexpr uint32_t kRegrantForInitialWindow =
    (kInitialRadioboundCredit / kRadioboundCreditIncrement) * kRadioboundCreditIncrement;

// Re-grant periods to observe: enough to show the +increment grant is periodic, not a one-off.
constexpr uint32_t kReplenishRounds = 3;
// Consecutive raw transmissions: the second proves the radio returned to Receive after TxDone.
constexpr uint8_t kTransmitCount = 2;

uint64_t ExpectedEui64(uint32_t node_id) { return Platform::kEui64Base | node_id; }

// The far end of the RCP's Radio channel: accepts Attach, records every Transmit (replying at once
// as the real medium does), and delivers the frames the test injects to the pending Receive.
class FakeMedium : public fidl::Server<rfsim::Medium> {
 public:
  // A frame some node passed to `Radio.Transmit`.
  struct Transmission {
    uint32_t node_id;
    rfsim::Frame frame;
  };

  explicit FakeMedium(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {}

  void Bind(fidl::ServerEnd<rfsim::Medium> server_end) {
    bindings_.AddBinding(dispatcher_, std::move(server_end), this, fidl::kIgnoreBindingClosure);
  }

  // test.lowpan.rfsim/Medium
  void Attach(AttachRequest& request, AttachCompleter::Sync& completer) override {
    attached_node_ids_.push_back(request.node_id());
    radios_.push_back(
        std::make_unique<Radio>(*this, request.node_id(), std::move(request.radio())));
    completer.Reply(fit::ok());
  }

  // Node ids passed to `Attach`, in order.
  const std::vector<uint32_t>& attached_node_ids() const { return attached_node_ids_; }
  // Every transmission, oldest first.
  const std::vector<Transmission>& transmissions() const { return transmissions_; }
  // How many `Radio.Receive` calls are outstanding.
  size_t pending_receives() const {
    return static_cast<size_t>(std::count_if(radios_.begin(), radios_.end(),
                                             [](const auto& radio) { return radio->receiving(); }));
  }

  // Replies to the first attached node's pending `Receive` with `frame`. Returns false if no
  // `Receive` is pending.
  bool Deliver(rfsim::Frame frame, int8_t rssi_dbm, uint8_t lqi) {
    return !radios_.empty() && radios_.front()->Deliver(std::move(frame), rssi_dbm, lqi);
  }

 private:
  class Radio : public fidl::Server<rfsim::Radio> {
   public:
    Radio(FakeMedium& medium, uint32_t node_id, fidl::ServerEnd<rfsim::Radio> server_end)
        : medium_(medium),
          node_id_(node_id),
          binding_(medium.dispatcher_, std::move(server_end), this, fidl::kIgnoreBindingClosure) {}

    // test.lowpan.rfsim/Radio
    void Transmit(TransmitRequest& request, TransmitCompleter::Sync& completer) override {
      medium_.transmissions_.push_back({
          .node_id = node_id_,
          .frame = rfsim::Frame{{
              .channel = request.channel(),
              .psdu = std::move(request.psdu()),
              .tx_power_dbm = request.tx_power_dbm(),
          }},
      });
      completer.Reply();
    }
    void Receive(ReceiveCompleter::Sync& completer) override {
      EXPECT_FALSE(pending_receive_.has_value()) << "second outstanding Radio.Receive";
      pending_receive_ = completer.ToAsync();
    }

    bool receiving() const { return pending_receive_.has_value(); }

    bool Deliver(rfsim::Frame frame, int8_t rssi_dbm, uint8_t lqi) {
      if (!pending_receive_.has_value()) {
        return false;
      }
      ReceiveCompleter::Async completer = std::move(*pending_receive_);
      pending_receive_.reset();
      completer.Reply({{.frame = std::move(frame), .rssi_dbm = rssi_dbm, .lqi = lqi}});
      return true;
    }

   private:
    FakeMedium& medium_;
    const uint32_t node_id_;
    std::optional<ReceiveCompleter::Async> pending_receive_;
    // Last, so it is torn down before the pending completer.
    fidl::ServerBinding<rfsim::Radio> binding_;
  };

  async_dispatcher_t* const dispatcher_;
  std::vector<uint32_t> attached_node_ids_;
  std::vector<Transmission> transmissions_;
  std::vector<std::unique_ptr<Radio>> radios_;
  fidl::ServerBindingGroup<rfsim::Medium> bindings_;
};

class SpinelDeviceServerTest : public MockSpinelHostFixture {
 protected:
  void TearDown() override {
    // Unbind DeviceSetup before destroying the server it dispatches to. The server, and with it
    // the RfsimClient it owns, goes before the medium, because RfsimClient treats a closed Radio
    // channel as fatal. Then let the client-side teardowns run.
    setup_binding_.reset();
    server_.reset();
    medium_.reset();
    RunLoopUntilIdle();
    MockSpinelHostFixture::TearDown();
  }

  // Creates the medium on the test loop and attaches an RfsimClient to it, then creates the server
  // with that client as its radio medium and connects `setup_`. As in main(), the RCP is not served
  // before it has attached.
  void StartRcp(uint32_t node_id = kNodeId,
                SpinelDeviceServer::FlowControl flow_control = kFlowControl) {
    node_id_ = node_id;
    medium_.emplace(dispatcher());
    auto [medium_client, medium_server] = fidl::Endpoints<rfsim::Medium>::Create();
    medium_->Bind(std::move(medium_server));
    std::unique_ptr<RfsimClient> rfsim;
    RfsimClient::Attach(
        dispatcher(), node_id, std::move(medium_client),
        [&rfsim](std::unique_ptr<RfsimClient> attached) { rfsim = std::move(attached); });
    RunLoopUntil([&rfsim] { return rfsim != nullptr; });

    server_.emplace(dispatcher(), node_id, flow_control, std::move(rfsim));
    auto [client_end, server_end] = fidl::Endpoints<fspinel::DeviceSetup>::Create();
    setup_binding_.emplace(dispatcher(), std::move(server_end), &*server_,
                           fidl::kIgnoreBindingClosure);
    setup_.Bind(std::move(client_end), dispatcher());
  }

  // The base fixture's SetChannel/OpenDevice/CloseDevice/GetMaxFrameSize use synchronous calls,
  // which would deadlock with the server on the same thread. These hide them with asynchronous
  // equivalents that pump the loop until the reply arrives; the rest of the base fixture only uses
  // one-way calls and is used unchanged.

  // `after_send`, if set, runs once the request has been written but before the loop runs, so
  // anything it does reaches the server after the SetChannel.
  zx_status_t SetChannel(MockSpinelHost& host, fit::function<void()> after_send = nullptr) {
    auto [client_end, server_end] = fidl::Endpoints<fspinel::Device>::Create();
    std::optional<zx_status_t> status;
    setup_->SetChannel({std::move(server_end)})
        .Then([&status](fidl::Result<fspinel::DeviceSetup::SetChannel>& result) {
          if (result.is_ok()) {
            status = ZX_OK;
          } else if (result.error_value().is_domain_error()) {
            status = result.error_value().domain_error();
          } else {
            status = result.error_value().framework_error().status();
          }
        });
    if (after_send) {
      after_send();
    }
    RunLoopUntil([&status] { return status.has_value(); });
    if (*status == ZX_OK) {
      host.Bind(std::move(client_end), dispatcher());
    }
    return *status;
  }

  // SetChannel(), retried while the server answers ALREADY_BOUND (see RetryWhileAlreadyBound).
  zx_status_t SetChannelWithRetry(MockSpinelHost& host) {
    return RetryWhileAlreadyBound([&] { return SetChannel(host); });
  }

  void OpenDevice(MockSpinelHost& host) {
    host.ResetSendWindow();
    std::optional<std::string> error;
    bool done = false;
    host.client()->Open().Then([&](fidl::Result<fspinel::Device::Open>& result) {
      if (result.is_error()) {
        error = result.error_value().FormatDescription();
      }
      done = true;
    });
    RunLoopUntil([&done] { return done; });
    ASSERT_FALSE(error.has_value()) << "Open() failed: " << *error;
    // Events sent before the reply (the initial grant) were dispatched before it; this also runs
    // whatever the RCP posted while resetting.
    RunLoopUntilIdle();
  }

  void CloseDevice(MockSpinelHost& host) {
    std::optional<std::string> error;
    bool done = false;
    host.client()->Close().Then([&](fidl::Result<fspinel::Device::Close>& result) {
      if (result.is_error()) {
        error = result.error_value().FormatDescription();
      }
      done = true;
    });
    RunLoopUntil([&done] { return done; });
    ASSERT_FALSE(error.has_value()) << "Close() failed: " << *error;
    RunLoopUntilIdle();
  }

  std::optional<uint32_t> GetMaxFrameSize(MockSpinelHost& host) {
    std::optional<uint32_t> size;
    bool done = false;
    host.client()->GetMaxFrameSize().Then(
        [&](fidl::Result<fspinel::Device::GetMaxFrameSize>& result) {
          if (result.is_ok()) {
            size = result->size();
          }
          done = true;
        });
    RunLoopUntil([&done] { return done; });
    return size;
  }

  // Creates a fresh server and a bound host.
  void StartAndBind(MockSpinelHost& host, uint32_t node_id = kNodeId) {
    ASSERT_NO_FATAL_FAILURE(StartRcp(node_id));
    ASSERT_EQ(SetChannelWithRetry(host), ZX_OK);
  }

  // Open, grant the inbound window and sync on the reset notification: the state in which the
  // host starts talking Spinel.
  void OpenAndSync(MockSpinelHost& host) {
    ASSERT_NO_FATAL_FAILURE(OpenDevice(host));
    ASSERT_NO_FATAL_FAILURE(GrantInboundWindowAndWaitForRcpToBeReady(host, kExpectedResetStatus));
  }

  // StartAndBind + OpenAndSync.
  void StartBindAndOpen(MockSpinelHost& host) {
    ASSERT_NO_FATAL_FAILURE(StartAndBind(host));
    ASSERT_NO_FATAL_FAILURE(OpenAndSync(host));
  }

  // Checks that the RCP attached to the medium exactly once, with its node id.
  void WaitForAttach() {
    EXPECT_EQ(medium_->attached_node_ids(), std::vector<uint32_t>{node_id_})
        << "ot-rcp-fuchsia must attach exactly once";
  }

  // Waits until the medium has recorded `count` transmissions, and asserts there are no more. The
  // medium shares the test loop, so once it is idle every transmission the RCP made has been
  // recorded.
  void WaitForExactTransmissions(size_t count) {
    RunLoopUntil([&] { return medium_->transmissions().size() >= count || HasFatalFailure(); });
    ASSERT_FALSE(HasFatalFailure());
    RunLoopUntilIdle();
    ASSERT_EQ(medium_->transmissions().size(), count) << "unexpected extra transmissions";
  }

  uint32_t node_id() const { return node_id_; }
  // Everything the medium has recorded so far, oldest first.
  const std::vector<FakeMedium::Transmission>& transmissions() const {
    return medium_->transmissions();
  }
  FakeMedium& medium() {
    ZX_ASSERT(medium_.has_value());
    return *medium_;
  }

 private:
  uint32_t node_id_ = 0;
  // Declaration order matters (members are destroyed bottom-up, as in TearDown): the medium
  // outlives the server, which owns the RfsimClient attached to it, and the server outlives the
  // binding that points to it.
  std::optional<FakeMedium> medium_;
  std::optional<SpinelDeviceServer> server_;
  std::optional<fidl::ServerBinding<fspinel::DeviceSetup>> setup_binding_;
  fidl::Client<fspinel::DeviceSetup> setup_;
};

// Succeeds iff `payload` is a HWADDR response carrying the EUI-64 derived from `node_id`.
::testing::AssertionResult IsHwAddrFor(const std::optional<std::vector<uint8_t>>& payload,
                                       uint32_t node_id) {
  if (!payload.has_value()) {
    return ::testing::AssertionFailure() << "no HWADDR payload";
  }
  std::optional<uint64_t> actual = UnpackEui64(*payload);
  if (!actual.has_value()) {
    return ::testing::AssertionFailure() << "HWADDR payload is not an EUI-64";
  }
  const uint64_t expected = ExpectedEui64(node_id);
  if (*actual != expected) {
    return ::testing::AssertionFailure() << std::hex << "eui64=0x" << *actual << ", expected 0x"
                                         << expected << " (node_id=0x" << node_id << ")";
  }
  return ::testing::AssertionSuccess();
}

// Exercise Host-RCP handshake sequence.
TEST_F(SpinelDeviceServerTest, HostInitHandshake) {
  MockSpinelHost host;
  ASSERT_NO_FATAL_FAILURE(StartAndBind(host));

  // 1. GetMaxFrameSize: must fit the FIDL limit and cover the largest frame the host's spinel
  //    stack can produce.
  std::optional<uint32_t> max_frame_size = GetMaxFrameSize(host);
  ASSERT_TRUE(max_frame_size.has_value());
  EXPECT_EQ(*max_frame_size, kExpectedMaxFrameSize);
  EXPECT_LE(*max_frame_size, fspinel::kMaxFrameSize);
  EXPECT_GE(*max_frame_size, static_cast<uint32_t>(SPINEL_FRAME_MAX_SIZE));

  // 2. Open: the initial radio-bound grant is sent before the Open reply on the same channel, so
  //    it has arrived by now; then exactly one unsolicited LAST_STATUS(RESET_POWER_ON).
  ASSERT_NO_FATAL_FAILURE(OpenDevice(host));
  ASSERT_EQ(host.grants().size(), 1u) << "no OnReadyForSendFrames after Open";
  EXPECT_EQ(host.grants().front(), kInitialRadioboundCredit);
  host.set_auto_replenish(true);
  ASSERT_OK(host.ReadyToReceive(kHostInboundWindow));
  ASSERT_NO_FATAL_FAILURE(
      ExpectResetNotification(WaitForResetNotification(host), kExpectedResetStatus));

  // 3. Protocol Version.
  {
    std::optional<std::vector<uint8_t>> payload = Get(host, SPINEL_PROP_PROTOCOL_VERSION);
    ASSERT_TRUE(payload.has_value());
    unsigned int major = 0;
    unsigned int minor = 0;
    ASSERT_GT(spinel_datatype_unpack(payload->data(), static_cast<spinel_size_t>(payload->size()),
                                     SPINEL_DATATYPE_UINT_PACKED_S SPINEL_DATATYPE_UINT_PACKED_S,
                                     &major, &minor),
              0);
    EXPECT_EQ(major, static_cast<unsigned int>(SPINEL_PROTOCOL_VERSION_THREAD_MAJOR));
    EXPECT_EQ(minor, static_cast<unsigned int>(SPINEL_PROTOCOL_VERSION_THREAD_MINOR));
  }

  // 4. Ncp Version.
  {
    std::optional<std::vector<uint8_t>> payload = Get(host, SPINEL_PROP_NCP_VERSION);
    ASSERT_TRUE(payload.has_value());
    const char* version = nullptr;
    ASSERT_GT(spinel_datatype_unpack(payload->data(), static_cast<spinel_size_t>(payload->size()),
                                     SPINEL_DATATYPE_UTF8_S, &version),
              0);
    ASSERT_NE(version, nullptr);
    EXPECT_GT(strlen(version), 0u);
  }

  // 5. Spinel Capabilities.
  {
    std::optional<std::vector<uint8_t>> payload = Get(host, SPINEL_PROP_CAPS);
    ASSERT_TRUE(payload.has_value());
    std::vector<unsigned int> caps;
    const uint8_t* data = payload->data();
    spinel_size_t len = static_cast<spinel_size_t>(payload->size());
    while (len > 0) {
      unsigned int cap = 0;
      spinel_ssize_t used = spinel_datatype_unpack(data, len, SPINEL_DATATYPE_UINT_PACKED_S, &cap);
      ASSERT_GT(used, 0) << "malformed CAPS list";
      caps.push_back(cap);
      data += used;
      len -= static_cast<spinel_size_t>(used);
    }
    auto has = [&caps](unsigned int cap) {
      return std::find(caps.begin(), caps.end(), cap) != caps.end();
    };
    EXPECT_TRUE(has(SPINEL_CAP_CONFIG_RADIO)) << "the co-processor is not an RCP";
    EXPECT_TRUE(has(SPINEL_CAP_MAC_RAW));
    EXPECT_TRUE(has(SPINEL_CAP_RCP_API_VERSION));
    EXPECT_TRUE(has(SPINEL_CAP_RCP_MIN_HOST_API_VERSION));
  }

  // 6. API version.
  {
    std::optional<unsigned int> rcp_api_version = GetUintPacked(host, SPINEL_PROP_RCP_API_VERSION);
    ASSERT_TRUE(rcp_api_version.has_value());
    EXPECT_EQ(*rcp_api_version, static_cast<unsigned int>(SPINEL_RCP_API_VERSION));
    EXPECT_GE(*rcp_api_version,
              static_cast<unsigned int>(SPINEL_MIN_HOST_SUPPORTED_RCP_API_VERSION));
    std::optional<unsigned int> min_host_version =
        GetUintPacked(host, SPINEL_PROP_RCP_MIN_HOST_API_VERSION);
    ASSERT_TRUE(min_host_version.has_value());
    EXPECT_LE(*min_host_version, static_cast<unsigned int>(SPINEL_RCP_API_VERSION));
  }

  // 7. Radio Capabilities.
  {
    std::optional<unsigned int> radio_caps = GetUintPacked(host, SPINEL_PROP_RADIO_CAPS);
    ASSERT_TRUE(radio_caps.has_value());
    EXPECT_EQ(*radio_caps & kHostRequiredRadioCaps, kHostRequiredRadioCaps)
        << "missing required radio caps: 0x" << std::hex << (kHostRequiredRadioCaps & ~*radio_caps)
        << " (got 0x" << *radio_caps << ")";
  }

  // 8. Mac Address.
  EXPECT_TRUE(IsHwAddrFor(Get(host, SPINEL_PROP_HWADDR), node_id()));

  // 9. PHY_CHAN_SUPPORTED.
  {
    std::optional<std::vector<uint8_t>> payload = Get(host, SPINEL_PROP_PHY_CHAN_SUPPORTED);
    ASSERT_TRUE(payload.has_value());
    EXPECT_FALSE(payload->empty());
    for (uint8_t channel : *payload) {
      EXPECT_GE(channel, OT_RADIO_2P4GHZ_OQPSK_CHANNEL_MIN);
      EXPECT_LE(channel, OT_RADIO_2P4GHZ_OQPSK_CHANNEL_MAX);
    }
  }

  // 10. RCP_TIMESTAMP.
  {
    uint64_t t1 = 0;
    uint64_t t2 = 0;
    std::optional<std::vector<uint8_t>> p1 = Get(host, SPINEL_PROP_RCP_TIMESTAMP);
    ASSERT_TRUE(p1.has_value());
    ASSERT_GT(spinel_datatype_unpack(p1->data(), static_cast<spinel_size_t>(p1->size()),
                                     SPINEL_DATATYPE_UINT64_S, &t1),
              0);
    std::optional<std::vector<uint8_t>> p2 = Get(host, SPINEL_PROP_RCP_TIMESTAMP);
    ASSERT_TRUE(p2.has_value());
    ASSERT_GT(spinel_datatype_unpack(p2->data(), static_cast<spinel_size_t>(p2->size()),
                                     SPINEL_DATATYPE_UINT64_S, &t2),
              0);
    EXPECT_GE(t2, t1);
  }

  // 11. PHY_RX_SENSITIVITY.
  {
    std::optional<std::vector<uint8_t>> payload = Get(host, SPINEL_PROP_PHY_RX_SENSITIVITY);
    ASSERT_TRUE(payload.has_value());
    int8_t sensitivity = 0;
    EXPECT_GT(spinel_datatype_unpack(payload->data(), static_cast<spinel_size_t>(payload->size()),
                                     SPINEL_DATATYPE_INT8_S, &sensitivity),
              0);
  }

  // 12. SET RADIO_COEX_ENABLE.
  {
    std::optional<std::vector<uint8_t>> payload =
        Set(host, SPINEL_PROP_RADIO_COEX_ENABLE, PackSpinelValue(SPINEL_DATATYPE_BOOL_S, true));
    ASSERT_TRUE(payload.has_value());
    bool enabled = false;
    ASSERT_GT(spinel_datatype_unpack(payload->data(), static_cast<spinel_size_t>(payload->size()),
                                     SPINEL_DATATYPE_BOOL_S, &enabled),
              0);
    EXPECT_TRUE(enabled);
  }

  ExpectHealthy(host);
}

// Send a frame before call `Open()`.
TEST_F(SpinelDeviceServerTest, SendFrameBeforeOpenReportsClosed) {
  MockSpinelHost host;
  ASSERT_NO_FATAL_FAILURE(StartAndBind(host));

  ASSERT_OK(host.SendRaw(NoopFrame(host.NextTid())));
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([&] { return !host.errors().empty() || host.unbound(); },
                                        kTimeout));
  ASSERT_EQ(host.errors().size(), 1u);
  EXPECT_EQ(host.errors()[0].error, fspinel::Error::kClosed);
  EXPECT_FALSE(host.errors()[0].did_close);
  EXPECT_FALSE(host.unbound());
  EXPECT_EQ(host.received_count(), 0u);

  // The binding stays usable.
  ASSERT_NO_FATAL_FAILURE(OpenAndSync(host));
  ASSERT_NO_FATAL_FAILURE(ExpectResponsive(host));
  EXPECT_EQ(host.errors().size(), 1u);
  EXPECT_FALSE(host.unbound());
}

// Send a larger-than-expected spinel frame.
TEST_F(SpinelDeviceServerTest, OversizedFrameReportsOutboundFrameTooLarge) {
  MockSpinelHost host;
  ASSERT_NO_FATAL_FAILURE(StartBindAndOpen(host));
  std::optional<uint32_t> max_size = GetMaxFrameSize(host);
  ASSERT_TRUE(max_size.has_value());
  ASSERT_LT(*max_size, fspinel::kMaxFrameSize) << "cannot build an oversized frame";

  std::vector<uint8_t> oversized = NoopFrame(host.NextTid());
  oversized.resize(*max_size + 1, 0);
  ASSERT_OK(host.SendRaw(std::move(oversized)));
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([&] { return !host.errors().empty() || host.unbound(); },
                                        kTimeout));
  ASSERT_EQ(host.errors().size(), 1u);
  EXPECT_EQ(host.errors()[0].error, fspinel::Error::kOutboundFrameTooLarge);
  EXPECT_FALSE(host.errors()[0].did_close);
  EXPECT_FALSE(host.unbound());

  // A frame of exactly the maximum size is accepted (NOOP followed by padding that NcpBase
  // ignores), and the device keeps working.
  const uint8_t tid = host.NextTid();
  std::vector<uint8_t> max_frame = NoopFrame(tid);
  max_frame.resize(*max_size, 0);
  ASSERT_NO_FATAL_FAILURE(SendWhenReady(host, std::move(max_frame)));
  ASSERT_TRUE(WaitForResponse(host, tid).has_value()) << "no response to a max-size frame";
  ASSERT_NO_FATAL_FAILURE(ExpectResponsive(host));
  EXPECT_EQ(host.errors().size(), 1u);
  EXPECT_FALSE(host.unbound());
}

// Host not complying with the flow control by sending one extra spinel frame without corresponding
// credit.
TEST_F(SpinelDeviceServerTest, ZeroRadioboundCreditClosesChannelWithIoOverrun) {
  MockSpinelHost host;
  ASSERT_NO_FATAL_FAILURE(StartAndBind(host));
  ASSERT_NO_FATAL_FAILURE(OpenDevice(host));
  ASSERT_EQ(host.grants().size(), 1u);
  ASSERT_EQ(host.total_granted(), kInitialRadioboundCredit);

  for (uint32_t i = 0; i < kInitialRadioboundCredit + 1; ++i) {
    ASSERT_OK(host.SendRaw(NoopFrame(host.NextTid())));
  }
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([&] { return host.unbound(); }, kTimeout))
      << "channel not closed after exceeding radio-bound credit";
  EXPECT_EQ(host.unbind_info()->status(), ZX_ERR_IO_OVERRUN)
      << host.unbind_info()->FormatDescription();
  EXPECT_TRUE(host.errors().empty()) << "overrun must close the channel, not emit OnError";
  EXPECT_EQ(host.total_granted(), kInitialRadioboundCredit);

  // After the server closed the binding, a new SetChannel works and the RCP is usable again.
  MockSpinelHost host2;
  ASSERT_EQ(SetChannelWithRetry(host2), ZX_OK);
  ASSERT_NO_FATAL_FAILURE(OpenAndSync(host2));
  ASSERT_NO_FATAL_FAILURE(ExpectResponsive(host2));
  ExpectHealthy(host2);
}

// Verify in the happy path, the flow control credits re-grant in `ot-rcp-fuchsia` work as expected.
TEST_F(SpinelDeviceServerTest, RadioboundCreditReplenishedPerIncrementConsumed) {
  MockSpinelHost host;
  ASSERT_NO_FATAL_FAILURE(StartBindAndOpen(host));
  // The initial grant is sent inside Open(), before its reply, so it has arrived by now.
  ASSERT_EQ(host.grants().size(), 1u);
  ASSERT_EQ(host.total_granted(), kInitialRadioboundCredit);

  for (uint32_t round = 1; round <= kReplenishRounds; ++round) {
    // Consume exactly one re-grant period's worth of frames; each reply proves NcpBase consumed
    // the frame, which is what triggers the re-grant (receipt alone does not).
    for (uint32_t i = 0; i < kRadioboundCreditIncrement; ++i) {
      ASSERT_NO_FATAL_FAILURE(ExpectResponsive(host));
    }
    const uint64_t expected_total = kInitialRadioboundCredit + round * kRadioboundCreditIncrement;
    ASSERT_TRUE(
        RunLoopWithTimeoutOrUntil([&] { return host.total_granted() >= expected_total; }, kTimeout))
        << "round " << round;
    EXPECT_EQ(host.total_granted(), expected_total) << "round " << round;
  }

  // One initial grant plus one per round; every re-grant is exactly the increment.
  ASSERT_EQ(host.grants().size(), 1 + kReplenishRounds);
  EXPECT_EQ(host.grants().front(), kInitialRadioboundCredit);
  for (size_t i = 1; i < host.grants().size(); ++i) {
    EXPECT_EQ(host.grants()[i], kRadioboundCreditIncrement) << "grant " << i;
  }
  ExpectHealthy(host);
}

// Verify `ot-rcp-fuchsia` send inbound frame to client only when credits are granted.
TEST_F(SpinelDeviceServerTest, ZeroClientboundCreditWithholdsFramesUntilReady) {
  MockSpinelHost host;
  ASSERT_NO_FATAL_FAILURE(StartAndBind(host));
  ASSERT_NO_FATAL_FAILURE(OpenDevice(host));

  const uint8_t tid = host.NextTid();
  ASSERT_NO_FATAL_FAILURE(SendWhenReady(
      host, BuildSpinelFrame(tid, SPINEL_CMD_PROP_VALUE_GET, SPINEL_PROP_PROTOCOL_VERSION)));
  // No clientbound credit: nothing may be delivered.
  RunLoopUntilIdle();
  EXPECT_EQ(host.received_count(), 0u);

  // One credit: exactly one frame.
  ASSERT_OK(host.ReadyToReceive(1));
  RunLoopUntilIdle();
  EXPECT_EQ(host.received_count(), 1u);

  // Release the rest: both the reset notification and the response must arrive.
  ASSERT_OK(host.ReadyToReceive(kHostInboundWindow));
  ASSERT_NO_FATAL_FAILURE(
      ExpectResetNotification(WaitForResetNotification(host), kExpectedResetStatus));
  std::optional<SpinelFrame> response = WaitForResponse(host, tid);
  ASSERT_TRUE(response.has_value());
  EXPECT_TRUE(response->IsPropValueIs(SPINEL_PROP_PROTOCOL_VERSION)) << response->Describe();
  ExpectHealthy(host);
}

// `ot-rcp-fuchsia` should process the host to rcp spinel frames even if there's no inbound credit
// given by host. Responses should be delivered to host after receiving the inbound credits.
TEST_F(SpinelDeviceServerTest, RadioboundBacklogDrainsAfterReadyToReceive) {
  MockSpinelHost host;
  ASSERT_NO_FATAL_FAILURE(StartAndBind(host));
  ASSERT_NO_FATAL_FAILURE(OpenDevice(host));
  ASSERT_EQ(host.total_granted(), kInitialRadioboundCredit);

  std::vector<uint8_t> tids;
  for (uint32_t i = 0; i < kInitialRadioboundCredit; ++i) {
    tids.push_back(host.NextTid());
    ASSERT_OK(host.SendRaw(NoopFrame(tids.back())));
  }
  RunLoopUntilIdle();
  EXPECT_EQ(host.received_count(), 0u);
  EXPECT_EQ(host.total_granted(), kInitialRadioboundCredit)
      << "frames must not be consumed while clientbound-blocked";
  EXPECT_FALSE(host.unbound());

  host.set_auto_replenish(true);
  ASSERT_OK(host.ReadyToReceive(kHostInboundWindow));
  ASSERT_NO_FATAL_FAILURE(
      ExpectResetNotification(WaitForResetNotification(host), kExpectedResetStatus));
  for (uint8_t tid : tids) {
    ASSERT_NO_FATAL_FAILURE(ExpectLastStatus(WaitForResponse(host, tid), SPINEL_STATUS_OK))
        << "backlogged tid " << int{tid};
  }
  // Every frame of the backlog has now been consumed, and the loop being idle means every grant
  // the server sent has been dispatched: the total is exact, not a lower bound.
  RunLoopUntilIdle();
  EXPECT_EQ(host.total_granted(), kInitialRadioboundCredit + kRegrantForInitialWindow);
  ASSERT_NO_FATAL_FAILURE(ExpectResponsive(host));
  ExpectHealthy(host);
}

// Check `ot-rcp-fuchsia` handles `Open()`, `Close()` then `Open()`.
TEST_F(SpinelDeviceServerTest, CloseThenOpenResetsAgainAndServesRequests) {
  MockSpinelHost host;
  ASSERT_NO_FATAL_FAILURE(StartBindAndOpen(host));
  ASSERT_TRUE(Get(host, SPINEL_PROP_PROTOCOL_VERSION).has_value());

  ASSERT_NO_FATAL_FAILURE(CloseDevice(host));
  // Closed again: same CLOSED semantics as before the first Open.
  host.ClearErrors();
  ASSERT_OK(host.SendRaw(NoopFrame(host.NextTid())));
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([&] { return !host.errors().empty(); }, kTimeout));
  ASSERT_EQ(host.errors().size(), 1u);
  EXPECT_EQ(host.errors()[0].error, fspinel::Error::kClosed);
  EXPECT_FALSE(host.errors()[0].did_close);

  host.ClearReceived();
  host.ClearGrants();
  host.set_auto_replenish(false);
  ASSERT_NO_FATAL_FAILURE(OpenAndSync(host));
  ASSERT_EQ(host.grants().size(), 1u);
  EXPECT_EQ(host.grants().front(), kInitialRadioboundCredit);

  ASSERT_TRUE(Get(host, SPINEL_PROP_PROTOCOL_VERSION).has_value());
  ASSERT_NO_FATAL_FAILURE(ExpectResponsive(host));
  EXPECT_EQ(host.errors().size(), 1u) << "only the CLOSED error from the closed window";
  EXPECT_FALSE(host.unbound());
}

// Check `ot-rcp-fuchsia` handles `Open()` then `Open()`. `Open()` should reset RCP.
TEST_F(SpinelDeviceServerTest, OpenWhileOpenTriggersReset) {
  MockSpinelHost host;
  ASSERT_NO_FATAL_FAILURE(StartBindAndOpen(host));
  ASSERT_NO_FATAL_FAILURE(ExpectResponsive(host));

  auto get_coex = [&]() -> std::optional<bool> {
    std::optional<std::vector<uint8_t>> payload = Get(host, SPINEL_PROP_RADIO_COEX_ENABLE);
    bool enabled = false;
    if (!payload.has_value() ||
        spinel_datatype_unpack(payload->data(), static_cast<spinel_size_t>(payload->size()),
                               SPINEL_DATATYPE_BOOL_S, &enabled) <= 0) {
      return std::nullopt;
    }
    return enabled;
  };
  const std::optional<bool> initial = get_coex();
  ASSERT_TRUE(initial.has_value());
  const bool toggled = !*initial;
  ASSERT_TRUE(
      Set(host, SPINEL_PROP_RADIO_COEX_ENABLE, PackSpinelValue(SPINEL_DATATYPE_BOOL_S, toggled))
          .has_value());
  ASSERT_EQ(get_coex(), std::optional<bool>(toggled));

  host.ClearReceived();
  host.set_auto_replenish(false);
  ASSERT_NO_FATAL_FAILURE(OpenAndSync(host));
  EXPECT_EQ(get_coex(), initial) << "coex flag survived Open (soft reset)";
  ASSERT_NO_FATAL_FAILURE(ExpectResponsive(host));
  ExpectHealthy(host);
}

// A second host try to connect to `ot-rcp-fuchsia`, should reject.
TEST_F(SpinelDeviceServerTest, SecondSetChannelWhileBoundFailsWithAlreadyBound) {
  MockSpinelHost host;
  ASSERT_NO_FATAL_FAILURE(StartAndBind(host));

  MockSpinelHost intruder;
  EXPECT_EQ(SetChannel(intruder), ZX_ERR_ALREADY_BOUND);

  // The original binding is unaffected.
  ASSERT_NO_FATAL_FAILURE(OpenAndSync(host));
  ASSERT_NO_FATAL_FAILURE(ExpectResponsive(host));
  EXPECT_EQ(SetChannel(intruder), ZX_ERR_ALREADY_BOUND);
  ExpectHealthy(host);
}

// The host dropped the FIDL connection and try to reconnect to `ot-rcp-fuchsia`.
TEST_F(SpinelDeviceServerTest, SetChannelSucceedsAfterBindingDropped) {
  auto host = std::make_unique<MockSpinelHost>();
  ASSERT_NO_FATAL_FAILURE(StartBindAndOpen(*host));
  ASSERT_NO_FATAL_FAILURE(ExpectResponsive(*host));

  host.reset();
  RunLoopUntilIdle();

  // The server notices PEER_CLOSED on the old Device channel and accepts the new one.
  MockSpinelHost host2;
  ASSERT_EQ(SetChannelWithRetry(host2), ZX_OK);
  ASSERT_NO_FATAL_FAILURE(OpenAndSync(host2));
  ASSERT_TRUE(Get(host2, SPINEL_PROP_PROTOCOL_VERSION).has_value());
  ExpectHealthy(host2);
}

// The server releases a dropped Device channel only once it has handled the close. A SetChannel
// that it handles first is rejected with ALREADY_BOUND, and the client's retry then succeeds.
TEST_F(SpinelDeviceServerTest, SetChannelRacingBindingDropIsRetried) {
  auto host = std::make_unique<MockSpinelHost>();
  ASSERT_NO_FATAL_FAILURE(StartBindAndOpen(*host));

  // The new SetChannel is written before the old client is dropped, so the dispatcher sees the
  // SetChannel before the old channel's PEER_CLOSED and the server still has it bound.
  MockSpinelHost host2;
  EXPECT_EQ(SetChannel(host2, [&host] { host.reset(); }), ZX_ERR_ALREADY_BOUND);
  ASSERT_EQ(host, nullptr);

  ASSERT_EQ(SetChannelWithRetry(host2), ZX_OK);
  ASSERT_NO_FATAL_FAILURE(OpenAndSync(host2));
  ASSERT_NO_FATAL_FAILURE(ExpectResponsive(host2));
  ExpectHealthy(host2);
}

// The RCP attaches with its node id and every raw transmission reaches the Medium's Radio with the
// channel, PSDU (FCS included, forwarded as is) and transmit power the host configured, while the
// host still gets its TX completion.
TEST_F(SpinelDeviceServerTest, RawTransmitIsForwardedToMedium) {
  constexpr uint16_t kPanId = 0xface;
  constexpr uint16_t kShortAddress = 0x0002;
  // Not the first channel, so a forwarded channel of 11 would be a coincidence, not a pass.
  constexpr uint8_t kChannel = OT_RADIO_2P4GHZ_OQPSK_CHANNEL_MIN + 4;
  constexpr int8_t kTxPower = -7;

  MockSpinelHost host;
  ASSERT_NO_FATAL_FAILURE(StartBindAndOpen(host));
  ASSERT_NO_FATAL_FAILURE(WaitForAttach());
  ASSERT_NO_FATAL_FAILURE(EnableRawLink(host, kPanId, kShortAddress, kChannel));
  ASSERT_TRUE(Set(host, SPINEL_PROP_PHY_TX_POWER, PackSpinelValue(SPINEL_DATATYPE_INT8_S, kTxPower))
                  .has_value());
  EXPECT_TRUE(transmissions().empty()) << "nothing was transmitted yet";

  for (uint8_t seq = 1; seq <= kTransmitCount; ++seq) {
    SCOPED_TRACE(int{seq});
    const std::vector<uint8_t> psdu = BroadcastDataFrame(kPanId, kShortAddress, seq);
    std::optional<SpinelFrame> response = Transact(
        host, SPINEL_CMD_PROP_VALUE_SET, SPINEL_PROP_STREAM_RAW, StreamRawValue(psdu, kChannel));
    EXPECT_EQ(StreamRawTxDoneStatus(response), std::optional<unsigned int>(SPINEL_STATUS_OK));

    ASSERT_NO_FATAL_FAILURE(WaitForExactTransmissions(seq));
    const FakeMedium::Transmission& tx = transmissions().back();
    EXPECT_EQ(tx.node_id, node_id());
    EXPECT_EQ(tx.frame.channel(), kChannel);
    EXPECT_EQ(tx.frame.psdu(), psdu);
    EXPECT_EQ(tx.frame.tx_power_dbm(), kTxPower);
  }

  // Attaching exactly once is checked by WaitForAttach.
  ASSERT_NO_FATAL_FAILURE(ExpectResponsive(host));
  ExpectHealthy(host);
}

// The Medium observes SubMac's software retransmissions: an AR=1 frame that nobody acknowledges
// is transmitted `1 + max_frame_retries` times before the host gets NO_ACK.
TEST_F(SpinelDeviceServerTest, UnicastAckRequestedTransmitIsRetriedOnMedium) {
  constexpr uint16_t kPanId = 0xface;
  constexpr uint16_t kSrcShort = 0x0002;
  constexpr uint16_t kDstShort = 0x0003;
  constexpr uint8_t kChannel = OT_RADIO_2P4GHZ_OQPSK_CHANNEL_MIN + 2;
  constexpr uint8_t kMaxFrameRetries = 2;
  constexpr size_t kExpectedAttempts = 1 + kMaxFrameRetries;

  MockSpinelHost host;
  ASSERT_NO_FATAL_FAILURE(StartBindAndOpen(host));
  ASSERT_NO_FATAL_FAILURE(WaitForAttach());
  ASSERT_NO_FATAL_FAILURE(EnableRawLink(host, kPanId, kSrcShort, kChannel));

  const std::vector<uint8_t> psdu =
      UnicastAckRequestedDataFrame(kPanId, kDstShort, kSrcShort, /*seq=*/0x42);
  std::optional<SpinelFrame> response =
      Transact(host, SPINEL_CMD_PROP_VALUE_SET, SPINEL_PROP_STREAM_RAW,
               StreamRawValue(psdu, kChannel, kMaxFrameRetries));
  EXPECT_EQ(StreamRawTxDoneStatus(response), std::optional<unsigned int>(SPINEL_STATUS_NO_ACK));

  ASSERT_NO_FATAL_FAILURE(WaitForExactTransmissions(kExpectedAttempts));
  for (const FakeMedium::Transmission& tx : transmissions()) {
    EXPECT_EQ(tx.node_id, node_id());
    EXPECT_EQ(tx.frame.channel(), kChannel);
    EXPECT_EQ(tx.frame.psdu(), psdu);
  }

  ASSERT_NO_FATAL_FAILURE(ExpectResponsive(host));
  ExpectHealthy(host);
}

// A frame the medium delivers on the channel the RCP receives on reaches the host as an
// unsolicited STREAM_RAW, with the RSSI the medium decided; frames on another channel and
// acknowledgments do not.
TEST_F(SpinelDeviceServerTest, ReceivedFrameReachesHost) {
  constexpr uint16_t kPanId = 0xface;
  constexpr uint16_t kShortAddress = 0x0002;
  constexpr uint16_t kPeerShortAddress = 0x0005;
  constexpr uint8_t kChannel = OT_RADIO_2P4GHZ_OQPSK_CHANNEL_MIN + 4;
  constexpr int8_t kRssi = -60;
  constexpr uint8_t kLqi = 200;

  MockSpinelHost host;
  ASSERT_NO_FATAL_FAILURE(StartBindAndOpen(host));
  ASSERT_NO_FATAL_FAILURE(WaitForAttach());
  ASSERT_NO_FATAL_FAILURE(EnableRawLink(host, kPanId, kShortAddress, kChannel));

  auto deliver = [&](uint8_t channel, std::vector<uint8_t> psdu) {
    RunLoopUntil([&] { return medium().pending_receives() == 1 || HasFatalFailure(); });
    ASSERT_TRUE(
        medium().Deliver(rfsim::Frame{{.channel = channel, .psdu = std::move(psdu)}}, kRssi, kLqi));
  };
  auto is_received_frame = [](const SpinelFrame& f) {
    return f.IsPropValueIs(SPINEL_PROP_STREAM_RAW) && f.tid() == 0;
  };

  // Dropped by the RCP: another channel, and an acknowledgment (frame type 2, sequence 1, FCS).
  ASSERT_NO_FATAL_FAILURE(
      deliver(kChannel + 1, BroadcastDataFrame(kPanId, kPeerShortAddress, /*seq=*/1)));
  ASSERT_NO_FATAL_FAILURE(deliver(kChannel, {0x02, 0x00, 0x01, 0x00, 0x00}));
  // Received.
  const std::vector<uint8_t> psdu = BroadcastDataFrame(kPanId, kPeerShortAddress, /*seq=*/2);
  ASSERT_NO_FATAL_FAILURE(deliver(kChannel, psdu));

  std::optional<SpinelFrame> received = WaitForFrame(host, is_received_frame);
  ASSERT_TRUE(received.has_value());
  const uint8_t* data = nullptr;
  spinel_size_t data_len = 0;
  int8_t rssi = 0;
  int8_t noise_floor = 0;
  uint16_t flags = 0;
  uint8_t rx_channel = 0;
  uint8_t lqi = 0;
  uint64_t timestamp = 0;
  ASSERT_GT(spinel_datatype_unpack(
                received->payload().data(), static_cast<spinel_size_t>(received->payload().size()),
                SPINEL_DATATYPE_DATA_WLEN_S SPINEL_DATATYPE_INT8_S SPINEL_DATATYPE_INT8_S
                    SPINEL_DATATYPE_UINT16_S SPINEL_DATATYPE_STRUCT_S(
                        SPINEL_DATATYPE_UINT8_S SPINEL_DATATYPE_UINT8_S SPINEL_DATATYPE_UINT64_S),
                &data, &data_len, &rssi, &noise_floor, &flags, &rx_channel, &lqi, &timestamp),
            0);
  EXPECT_EQ(std::vector<uint8_t>(data, data + data_len), psdu);
  EXPECT_EQ(rssi, kRssi);
  EXPECT_EQ(rx_channel, kChannel);
  EXPECT_EQ(lqi, kLqi);

  RunLoopUntilIdle();
  EXPECT_FALSE(host.TakeFrame(is_received_frame).has_value())
      << "only the frame on the receive channel may reach the host";
  EXPECT_EQ(medium().pending_receives(), 1u);
  ASSERT_NO_FATAL_FAILURE(ExpectResponsive(host));
  ExpectHealthy(host);
}

}  // namespace
}  // namespace ot_rcp::testing
