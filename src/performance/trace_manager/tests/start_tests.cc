// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/event.h>
#include <lib/zx/port.h>
#include <lib/zx/socket.h>

#include <algorithm>
#include <string>
#include <thread>
#include <variant>
#include <vector>

#include <gmock/gmock.h>
#include <trace-reader/records.h>

#include "src/performance/trace_manager/tests/trace_manager_test.h"

namespace tracing {
namespace test {

using SessionState = TraceManagerTest::SessionState;

// This only works when no other condition could cause the loop to exit.
// E.g., This doesn't work if the state is kStopping or kTerminating as the
// transition to kStopped,kTerminated will also cause the loop to exit.
template <typename T>
fit::result<fuchsia_tracing_controller::StartError> TryStart(TraceManagerTest* fixture,
                                                             const T& interface_ptr) {
  fit::result<fuchsia_tracing_controller::StartError> start_result = fit::ok();
  fuchsia_tracing_controller::StartOptions start_options{
      TraceManagerTest::GetDefaultStartOptions()};
  bool start_completed = false;
  interface_ptr->StartTracing({{std::move(start_options)}})
      .ThenExactlyOnce(
          [fixture, &start_completed,
           &start_result](fidl::Result<fuchsia_tracing_controller::Session::StartTracing>& result) {
            start_completed = true;
            if (result.is_error()) {
              if (result.error_value().is_domain_error()) {
                start_result = fit::error(result.error_value().domain_error());
              } else {
                start_result = fit::error(fuchsia_tracing_controller::StartError::kNotInitialized);
              }
            } else {
              start_result = fit::ok();
            }
            fixture->QuitLoop();
          });
  fixture->RunLoopUntilIdle();
  FX_LOGS(DEBUG) << "Start loop done";
  EXPECT_TRUE(start_completed);
  return start_result;
}

template <typename T>
void TryExtraStart(TraceManagerTest* fixture, const T& interface_ptr) {
  auto start_result{TryStart(fixture, interface_ptr)};
  EXPECT_EQ(fixture->GetSessionState(), SessionState::kStarted);
  EXPECT_TRUE(start_result.is_error());
  EXPECT_EQ(start_result.error_value(), fuchsia_tracing_controller::StartError::kAlreadyStarted);
}

TEST_F(TraceManagerTest, ExtraStart) {
  ConnectToProvisionerService();

  EXPECT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name));

  ASSERT_TRUE(InitializeSession());

  ASSERT_TRUE(StartSession());

  // Now try starting again.
  TryExtraStart(this, session_client());
}

TEST_F(TraceManagerTest, StartWhileStopping) {
  ConnectToProvisionerService();

  EXPECT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name));

  ASSERT_TRUE(InitializeSession());

  ASSERT_TRUE(StartSession());

  fuchsia_tracing_controller::StopOptions stop_options{GetDefaultStopOptions()};
  session_client()->StopTracing({{std::move(stop_options)}}).ThenExactlyOnce([](auto& result) {
    ASSERT_TRUE(result.is_ok());
  });
  RunLoopUntilIdle();
  // The loop will exit for the transition to kStopping.
  FX_LOGS(DEBUG) << "Loop done, expecting session stopping";
  ASSERT_EQ(GetSessionState(), SessionState::kStopping);

  // Now try a Start while we're still in |kStopping|.
  // The provider doesn't advance state until we tell it to, so we should
  // still remain in |kStopping|.
  fit::result<fuchsia_tracing_controller::StartError> start_result = fit::ok();
  fuchsia_tracing_controller::StartOptions start_options{GetDefaultStartOptions()};
  bool start_completed = false;
  session_client()
      ->StartTracing({{std::move(start_options)}})
      .ThenExactlyOnce(
          [this, &start_completed,
           &start_result](fidl::Result<fuchsia_tracing_controller::Session::StartTracing>& result) {
            start_completed = true;
            if (result.is_error()) {
              if (result.error_value().is_domain_error()) {
                start_result = fit::error(result.error_value().domain_error());
              } else {
                start_result = fit::error(fuchsia_tracing_controller::StartError::kNotInitialized);
              }
            } else {
              start_result = fit::ok();
            }
            QuitLoop();
          });
  RunLoopUntilIdle();
  FX_LOGS(DEBUG) << "Start loop done";
  EXPECT_TRUE(GetSessionState() == SessionState::kStopping);
  ASSERT_TRUE(start_completed);
  ASSERT_TRUE(start_result.is_error());
  EXPECT_EQ(start_result.error_value(), fuchsia_tracing_controller::StartError::kStopping);

  MarkAllProvidersStopped();
  RunLoopUntilIdle();
}

namespace {

using ::testing::ElementsAre;

fuchsia_tracing_controller::TraceConfig StreamingConfig() {
  fuchsia_tracing_controller::TraceConfig config = TraceManagerTest::GetDefaultTraceConfig();
  config.buffering_mode(fuchsia_tracing::BufferingMode::kStreaming);
  return config;
}

// Sends StartTracing and sets |*replied| when the reply arrives, whatever its result.
void BeginStartTracing(TraceManagerTest* fixture, bool* replied) {
  fixture->session_client()
      ->StartTracing({{TraceManagerTest::GetDefaultStartOptions()}})
      .ThenExactlyOnce([replied](auto& result) { *replied = true; });
}

}  // namespace

// https://fxbug.dev/581533585: Consumers may wait for the StartTracing reply before they read the
// socket, so a buffer save that arrives while the session is starting must not be written (or
// acked) until the reply has been sent.
TEST_F(TraceManagerTest, V1SaveWhileStartingIsWrittenAfterStartReply) {
  ConnectToProvisionerService();
  FakeProvider* provider = nullptr;
  ASSERT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name, &provider));
  ASSERT_TRUE(InitializeSession(StreamingConfig()));
  ReadRecordsFromSocket();  // Discard the magic number record.

  BeginStartSession();
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarting);

  provider->SendSaveBuffer(/*wrapped_count=*/0, /*durable_data_end=*/16);
  RunLoopUntilIdle();
  EXPECT_TRUE(IsSocketEmpty());
  EXPECT_EQ(provider->buffer_saved_count(), 0);

  ASSERT_TRUE(FinishStartSession());
  EXPECT_FALSE(IsSocketEmpty());
  EXPECT_EQ(provider->buffer_saved_count(), 1);
  EXPECT_EQ(provider->terminate_count(), 0);

  ASSERT_TRUE(TerminateSession());
}

// Same as above for V2 providers, with two saves that must be performed in order.
TEST_F(TraceManagerTest, V2SavesWhileStartingAreWrittenAfterStartReply) {
  ConnectToProvisionerService();
  FakeProviderV2* provider = nullptr;
  ASSERT_TRUE(AddFakeProviderV2(kProvider1Pid, kProvider1Name, &provider));
  ASSERT_TRUE(InitializeSession(StreamingConfig()));
  ReadRecordsFromSocket();  // Discard the magic number record.

  BeginStartSession();
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarting);

  provider->SendOnSaveBuffer(/*wrapped_count=*/0, /*durable_data_end=*/16);
  provider->SendOnSaveBuffer(/*wrapped_count=*/1, /*durable_data_end=*/16);
  RunLoopUntilIdle();
  EXPECT_TRUE(IsSocketEmpty());
  EXPECT_EQ(provider->notify_buffer_saved_count(), 0);

  ASSERT_TRUE(FinishStartSession());
  EXPECT_FALSE(IsSocketEmpty());
  // Performing the saves out of order would fail the wrapped_count check and terminate the
  // provider.
  EXPECT_EQ(provider->notify_buffer_saved_count(), 2);
  EXPECT_EQ(provider->terminate_count(), 0);

  ASSERT_TRUE(TerminateSession());
}

// With defer_transfer the forwarder writes to a file, not the socket, so a write can't wait for
// the consumer. Saves made while the session is starting run, and are acked, right away.
TEST_F(TraceManagerTest, SaveWhileStartingWithDeferTransferIsAckedBeforeStartReply) {
  ConnectToProvisionerService();
  FakeProviderV2* provider = nullptr;
  ASSERT_TRUE(AddFakeProviderV2(kProvider1Pid, kProvider1Name, &provider));
  fuchsia_tracing_controller::TraceConfig config = StreamingConfig();
  config.defer_transfer(true);
  ASSERT_TRUE(InitializeSession(std::move(config)));

  BeginStartSession();
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarting);

  provider->SendOnSaveBuffer(/*wrapped_count=*/0, /*durable_data_end=*/16);
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarting);
  EXPECT_EQ(provider->notify_buffer_saved_count(), 1);
  EXPECT_EQ(provider->terminate_count(), 0);

  ASSERT_TRUE(FinishStartSession());
  EXPECT_EQ(provider->notify_buffer_saved_count(), 1);
  ASSERT_TRUE(TerminateSession());
}

// A deferred save that fails when it runs after the StartTracing reply terminates the provider at
// that point, leaving it cleanly re-initializable for subsequent sessions.
TEST_F(TraceManagerTest, V2DeferredSaveErrorTerminatesProviderAfterStartReply) {
  ConnectToProvisionerService();
  FakeProviderV2* provider = nullptr;
  ASSERT_TRUE(AddFakeProviderV2(kProvider1Pid, kProvider1Name, &provider));
  ASSERT_TRUE(InitializeSession(StreamingConfig()));
  ReadRecordsFromSocket();  // Discard the magic number record.

  BeginStartSession();
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarting);

  // Send an invalid wrapped_count so TransferBuffer fails when the deferred save runs.
  provider->SendOnSaveBuffer(/*wrapped_count=*/99, /*durable_data_end=*/16);
  RunLoopUntilIdle();
  EXPECT_TRUE(IsSocketEmpty());
  EXPECT_EQ(provider->notify_buffer_saved_count(), 0);
  EXPECT_EQ(provider->terminate_count(), 0);
  EXPECT_EQ(provider->state(), FakeProviderV2::State::kStarting);

  ASSERT_TRUE(FinishStartSession());
  EXPECT_EQ(provider->notify_buffer_saved_count(), 0);
  EXPECT_EQ(provider->terminate_count(), 1);
  EXPECT_EQ(provider->state(), FakeProviderV2::State::kTerminating);

  provider->MarkTerminated();
  RunLoopUntilIdle();
  EXPECT_EQ(provider->state(), FakeProviderV2::State::kTerminated);

  DisconnectFromControllerService();
  RunLoopUntilIdle();
  EXPECT_EQ(GetSessionState(), SessionState::kNonexistent);

  provider->ResetCounts();
  ASSERT_TRUE(InitializeSession());
  EXPECT_EQ(provider->state(), FakeProviderV2::State::kInitialized);
  EXPECT_EQ(provider->initialize_count(), 1);
  ASSERT_TRUE(TerminateSession());
}

// A deferred save is also performed when the StartTracing reply is sent because a provider did
// not ack the start in time.
TEST_F(TraceManagerTest, SaveWhileStartingIsWrittenAfterStartTimeout) {
  ConnectToProvisionerService();
  FakeProviderV2* provider = nullptr;
  ASSERT_TRUE(AddFakeProviderV2(kProvider1Pid, kProvider1Name, &provider));
  fuchsia_tracing_controller::TraceConfig config = StreamingConfig();
  config.start_timeout_milliseconds(1000);
  ASSERT_TRUE(InitializeSession(std::move(config)));
  ReadRecordsFromSocket();  // Discard the magic number record.

  bool start_replied = false;
  BeginStartTracing(this, &start_replied);
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarting);

  provider->SendOnSaveBuffer(/*wrapped_count=*/0, /*durable_data_end=*/16);
  RunLoopUntilIdle();
  EXPECT_TRUE(IsSocketEmpty());
  EXPECT_EQ(provider->notify_buffer_saved_count(), 0);

  // The provider never acks the start.
  RunLoopFor(zx::msec(1000));
  EXPECT_TRUE(start_replied);
  EXPECT_EQ(GetSessionState(), SessionState::kStarted);
  EXPECT_FALSE(IsSocketEmpty());
  EXPECT_EQ(provider->notify_buffer_saved_count(), 1);

  ASSERT_TRUE(TerminateSession());
}

// Stopping while still starting performs the deferred save when the stop request arrives, right
// after StartTracing is completed.
TEST_F(TraceManagerTest, SaveWhileStartingIsWrittenOnStop) {
  ConnectToProvisionerService();
  FakeProviderV2* provider = nullptr;
  ASSERT_TRUE(AddFakeProviderV2(kProvider1Pid, kProvider1Name, &provider));
  ASSERT_TRUE(InitializeSession(StreamingConfig()));
  ReadRecordsFromSocket();  // Discard the magic number record.

  bool start_replied = false;
  BeginStartTracing(this, &start_replied);
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarting);

  provider->SendOnSaveBuffer(/*wrapped_count=*/0, /*durable_data_end=*/16);
  RunLoopUntilIdle();
  EXPECT_TRUE(IsSocketEmpty());
  EXPECT_EQ(provider->notify_buffer_saved_count(), 0);

  BeginStopSession();
  RunLoopUntilIdle();
  EXPECT_TRUE(start_replied);
  EXPECT_FALSE(IsSocketEmpty());
  EXPECT_EQ(provider->notify_buffer_saved_count(), 1);
  EXPECT_EQ(provider->terminate_count(), 0);
  ASSERT_TRUE(FinishStopSession());

  ASSERT_TRUE(TerminateSession());
}

// Terminating (the controller went away) while still starting performs the deferred save when
// termination begins.
TEST_F(TraceManagerTest, SaveWhileStartingIsWrittenOnTerminate) {
  ConnectToProvisionerService();
  FakeProvider* provider = nullptr;
  ASSERT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name, &provider));
  ASSERT_TRUE(InitializeSession(StreamingConfig()));
  ReadRecordsFromSocket();  // Discard the magic number record.

  BeginStartSession();
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarting);

  provider->SendSaveBuffer(/*wrapped_count=*/0, /*durable_data_end=*/16);
  RunLoopUntilIdle();
  EXPECT_TRUE(IsSocketEmpty());
  EXPECT_EQ(provider->buffer_saved_count(), 0);

  BeginTerminateSession();
  EXPECT_FALSE(IsSocketEmpty());
  EXPECT_EQ(provider->buffer_saved_count(), 1);
  ASSERT_TRUE(FinishTerminateSession());
}

namespace {

// Closes both of |provider|'s connections to trace_manager, as when its process exits.
void DisconnectProvider(TraceManagerTest* fixture, const void* provider) {
  for (const TraceManagerTest::ProviderBinding& binding : fixture->fake_provider_bindings()) {
    std::visit(
        [provider](const auto& b) {
          if (b->provider.get() == provider) {
            b->Disconnect();
          }
        },
        binding);
  }
}

std::vector<std::string> ProviderNamesInRecords(const std::vector<trace::Record>& records) {
  std::vector<std::string> names;
  for (const auto& record : records) {
    if (record.type() == trace::RecordType::kMetadata &&
        record.GetMetadata().type() == trace::MetadataType::kProviderInfo) {
      names.push_back(record.GetMetadata().content.GetProviderInfo().name);
    }
  }
  return names;
}

size_t CountBlobRecords(const std::vector<trace::Record>& records) {
  return std::ranges::count_if(records, [](const trace::Record& record) {
    return record.type() == trace::RecordType::kBlob;
  });
}

}  // namespace

// A V1 provider that terminates while the session is starting has its data written only once the
// StartTracing reply has been sent. That includes the data of a save that was deferred before it
// terminated.
TEST_F(TraceManagerTest, V1ProviderTerminatedWhileStartingIsWrittenAfterStartReply) {
  ConnectToProvisionerService();
  FakeProvider* provider1 = nullptr;
  ASSERT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name, &provider1));
  FakeProvider* provider2 = nullptr;
  ASSERT_TRUE(AddFakeProvider(kProvider2Pid, kProvider2Name, &provider2));
  ASSERT_TRUE(InitializeSession(StreamingConfig()));
  ReadRecordsFromSocket();  // Discard the magic number record.

  bool start_replied = false;
  BeginStartTracing(this, &start_replied);
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarting);

  // Like trace-engine, the provider switches to rolling buffer 1 and asks for buffer 0 to be
  // saved. Writing out the whole buffer on termination skips buffer 0, so only the deferred save
  // writes it. The provider exits right after asking, without letting the loop run in between:
  // the save must be recorded before trace_manager sees the FIFO peer close.
  provider1->SendSaveBuffer(/*wrapped_count=*/0, /*durable_data_end=*/16);
  // This removes the tracee and destroys its provider bundle.
  DisconnectProvider(this, provider1);
  RunLoopUntilIdle();
  // |provider2| hasn't acked the start, so the session is still starting.
  ASSERT_EQ(GetSessionState(), SessionState::kStarting);
  EXPECT_TRUE(IsSocketEmpty());

  provider2->MarkStarted();
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarted);
  EXPECT_TRUE(start_replied);
  std::vector<trace::Record> records = ReadRecordsFromSocket();
  EXPECT_THAT(ProviderNamesInRecords(records), ElementsAre(kProvider1Name));
  // The blob record |provider1| wrote when it started, exactly once.
  EXPECT_EQ(CountBlobRecords(records), 1u);

  DisconnectFromControllerService();
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kTerminating);
  provider2->MarkTerminated();
  RunLoopUntilIdle();
  EXPECT_EQ(GetSessionState(), SessionState::kNonexistent);
}

// Same as above for a V2 provider.
TEST_F(TraceManagerTest, V2ProviderTerminatedWhileStartingIsWrittenAfterStartReply) {
  ConnectToProvisionerService();
  FakeProviderV2* provider1 = nullptr;
  ASSERT_TRUE(AddFakeProviderV2(kProvider1Pid, kProvider1Name, &provider1));
  FakeProviderV2* provider2 = nullptr;
  ASSERT_TRUE(AddFakeProviderV2(kProvider2Pid, kProvider2Name, &provider2));
  ASSERT_TRUE(InitializeSession(StreamingConfig()));
  ReadRecordsFromSocket();  // Discard the magic number record.

  bool start_replied = false;
  BeginStartTracing(this, &start_replied);
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarting);

  ASSERT_NO_FATAL_FAILURE(provider1->WriteBlobRecords(/*num_bytes=*/1));
  provider1->SendOnSaveBuffer(/*wrapped_count=*/0, /*durable_data_end=*/16);
  RunLoopUntilIdle();
  // This removes the tracee and destroys its provider connection.
  DisconnectProvider(this, provider1);
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarting);
  EXPECT_TRUE(IsSocketEmpty());

  provider2->MarkStarted();
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarted);
  EXPECT_TRUE(start_replied);
  std::vector<trace::Record> records = ReadRecordsFromSocket();
  EXPECT_THAT(ProviderNamesInRecords(records), ElementsAre(kProvider1Name));
  EXPECT_EQ(CountBlobRecords(records), 1u);

  DisconnectFromControllerService();
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kTerminating);
  provider2->MarkTerminated();
  RunLoopUntilIdle();
  EXPECT_EQ(GetSessionState(), SessionState::kNonexistent);
}

// A provider that terminates while the session is starting, without having saved a buffer, also
// has its data written only once the StartTracing reply has been sent.
TEST_F(TraceManagerTest, ProviderTerminatedWhileStartingWithoutSaveIsWrittenAfterStartReply) {
  ConnectToProvisionerService();
  FakeProvider* provider1 = nullptr;
  ASSERT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name, &provider1));
  FakeProvider* provider2 = nullptr;
  ASSERT_TRUE(AddFakeProvider(kProvider2Pid, kProvider2Name, &provider2));
  ASSERT_TRUE(InitializeSession(StreamingConfig()));
  ReadRecordsFromSocket();  // Discard the magic number record.

  bool start_replied = false;
  BeginStartTracing(this, &start_replied);
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarting);

  // Closing the FIFO removes the tracee; the provider bundle stays registered.
  provider1->AdvanceToState(FakeProvider::State::kTerminated);
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarting);
  EXPECT_TRUE(IsSocketEmpty());

  provider2->MarkStarted();
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarted);
  EXPECT_TRUE(start_replied);
  std::vector<trace::Record> records = ReadRecordsFromSocket();
  EXPECT_THAT(ProviderNamesInRecords(records), ElementsAre(kProvider1Name));
  EXPECT_EQ(CountBlobRecords(records), 1u);

  DisconnectFromControllerService();
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kTerminating);
  provider2->MarkTerminated();
  RunLoopUntilIdle();
  EXPECT_EQ(GetSessionState(), SessionState::kNonexistent);
}

namespace {

void DrainSocket(const zx::socket& socket) {
  uint8_t buf[4096];
  size_t actual = 0;
  zx_status_t status;
  while ((status = socket.read(0, buf, sizeof(buf), &actual)) == ZX_OK) {
  }
  EXPECT_EQ(status, ZX_ERR_SHOULD_WAIT);
}

}  // namespace

// https://fxbug.dev/581533585: StopTracing sent while StartTracing is still pending completes
// StartTracing, then performs the deferred saves, then replies once stopped. A client that waits
// for the StartTracing reply before it reads the socket would otherwise deadlock against a full
// socket.
TEST_F(TraceManagerTest, StopWhileStartingRepliesToStartBeforeWritingDeferredSaves) {
  ConnectToProvisionerService();
  FakeProviderV2* provider = nullptr;
  ASSERT_TRUE(AddFakeProviderV2(kProvider1Pid, kProvider1Name, &provider));
  RunLoopUntilIdle();

  zx::socket consumer_socket, trace_manager_socket;
  ASSERT_EQ(zx::socket::create(0u, &consumer_socket, &trace_manager_socket), ZX_OK);
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_tracing_controller::Session>::Create();
  // Borrowed before |client| takes ownership, to see when trace_manager sends it a message.
  zx::unowned_channel controller_channel = client_end.channel().borrow();
  fidl::Client client(std::move(client_end), dispatcher());
  ASSERT_TRUE(provisioner_client()
                  ->InitializeTracing(
                      {{std::move(server_end), StreamingConfig(), std::move(trace_manager_socket)}})
                  .is_ok());
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kInitialized);
  DrainSocket(consumer_socket);  // The magic number record.

  std::vector<std::string> replies;
  client->StartTracing({{GetDefaultStartOptions()}}).ThenExactlyOnce([&replies](auto& result) {
    EXPECT_TRUE(result.is_ok());
    replies.emplace_back("start");
  });
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarting);

  provider->SendOnSaveBuffer(/*wrapped_count=*/0, /*durable_data_end=*/16);
  RunLoopUntilIdle();
  EXPECT_EQ(provider->notify_buffer_saved_count(), 0);

  client->StopTracing({{GetDefaultStopOptions()}}).ThenExactlyOnce([&replies](auto& result) {
    EXPECT_TRUE(result.is_ok());
    replies.emplace_back("stop");
  });
  // A port queues packets in the order their signals are asserted, which shows whether the
  // StartTracing reply or the deferred save's data was sent first.
  constexpr uint64_t kControllerKey = 1;
  constexpr uint64_t kSocketKey = 2;
  zx::port port;
  ASSERT_EQ(zx::port::create(0, &port), ZX_OK);
  ASSERT_EQ(controller_channel->wait_async(port, kControllerKey, ZX_CHANNEL_READABLE, 0), ZX_OK);
  ASSERT_EQ(consumer_socket.wait_async(port, kSocketKey, ZX_SOCKET_READABLE, 0), ZX_OK);
  RunLoopUntilIdle();
  EXPECT_EQ(GetSessionState(), SessionState::kStopping);
  zx_port_packet_t packet;
  ASSERT_EQ(port.wait(zx::time::infinite_past(), &packet), ZX_OK);
  EXPECT_EQ(packet.key, kControllerKey);
  ASSERT_EQ(port.wait(zx::time::infinite_past(), &packet), ZX_OK);
  EXPECT_EQ(packet.key, kSocketKey);
  EXPECT_THAT(replies, ElementsAre("start"));
  EXPECT_EQ(provider->notify_buffer_saved_count(), 1);

  provider->MarkStopped();
  RunLoopUntilIdle();
  EXPECT_THAT(replies, ElementsAre("start", "stop"));

  // Closing the controller terminates the session.
  client = {};
  RunLoopUntilIdle();
  EXPECT_EQ(GetSessionState(), SessionState::kTerminating);
  provider->MarkTerminated();
  RunLoopUntilIdle();
  EXPECT_EQ(GetSessionState(), SessionState::kNonexistent);
}

// trace_manager can go away while the session holds the writes for a provider that terminated
// while the session was starting. They are dropped without being performed.
TEST_F(TraceManagerTest, DestroyWithDeferredWritesOfTerminatedProvider) {
  ConnectToProvisionerService();
  FakeProvider* provider1 = nullptr;
  ASSERT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name, &provider1));
  ASSERT_TRUE(AddFakeProvider(kProvider2Pid, kProvider2Name));
  ASSERT_TRUE(InitializeSession(StreamingConfig()));
  ReadRecordsFromSocket();  // Discard the magic number record.

  BeginStartSession();
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarting);

  provider1->SendSaveBuffer(/*wrapped_count=*/0, /*durable_data_end=*/16);
  RunLoopUntilIdle();
  DisconnectProvider(this, provider1);
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarting);
  EXPECT_TRUE(IsSocketEmpty());

  DestroyTraceManager();
  RunLoopUntilIdle();
  EXPECT_TRUE(IsSocketEmpty());
}

// A save made while starting that is larger than the socket must not deadlock a consumer that
// reads the socket only after the StartTracing reply. Without the fix this test hangs.
TEST_F(TraceManagerTest, LargeSaveWhileStartingDoesNotDeadlock) {
  ConnectToProvisionerService();
  FakeProviderV2* provider = nullptr;
  ASSERT_TRUE(AddFakeProviderV2(kProvider1Pid, kProvider1Name, &provider));
  RunLoopUntilIdle();

  zx::socket consumer_socket, trace_manager_socket;
  ASSERT_EQ(zx::socket::create(0u, &consumer_socket, &trace_manager_socket), ZX_OK);
  zx_info_socket_t socket_info{};
  ASSERT_EQ(
      consumer_socket.get_info(ZX_INFO_SOCKET, &socket_info, sizeof(socket_info), nullptr, nullptr),
      ZX_OK);
  const size_t save_bytes = socket_info.rx_buf_max + 64 * 1024;

  fuchsia_tracing_controller::TraceConfig config = StreamingConfig();
  config.buffer_size_megabytes_hint(2);
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_tracing_controller::Session>::Create();
  ASSERT_TRUE(provisioner_client()
                  ->InitializeTracing(
                      {{std::move(server_end), std::move(config), std::move(trace_manager_socket)}})
                  .is_ok());
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kInitialized);
  // Let the provider finish initializing its buffer, then fill it with more than the socket holds.
  // Start() uses BufferDisposition::kRetain, so the records are kept.
  RunLoopUntilIdle();
  ASSERT_NO_FATAL_FAILURE(provider->WriteBlobRecords(save_bytes));

  zx::event start_sent, controller_closed;
  ASSERT_EQ(zx::event::create(0, &start_sent), ZX_OK);
  ASSERT_EQ(zx::event::create(0, &controller_closed), ZX_OK);

  // The consumer runs on its own thread, like a real client in another process. It reads the
  // socket only once StartTracing has replied, and until trace_manager closes the socket.
  bool start_ok = false;
  size_t bytes_read = 0;
  std::thread consumer([&, client_end = std::move(client_end)]() mutable {
    {
      async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
      fidl::Client client(std::move(client_end), loop.dispatcher());
      client->StartTracing({{GetDefaultStartOptions()}}).ThenExactlyOnce([&](auto& result) {
        start_ok = result.is_ok();
        loop.Quit();
      });
      EXPECT_EQ(start_sent.signal(0, ZX_USER_SIGNAL_0), ZX_OK);
      loop.Run();
      // Closing the controller terminates the session; it is torn down below.
      client = {};
      loop.Shutdown();
    }
    EXPECT_EQ(controller_closed.signal(0, ZX_USER_SIGNAL_0), ZX_OK);
    while (true) {
      zx_signals_t pending = 0;
      zx_status_t status = consumer_socket.wait_one(ZX_SOCKET_READABLE | ZX_SOCKET_PEER_CLOSED,
                                                    zx::time::infinite(), &pending);
      EXPECT_EQ(status, ZX_OK);
      uint8_t buf[4096];
      size_t actual = 0;
      while (consumer_socket.read(0, buf, sizeof(buf), &actual) == ZX_OK) {
        bytes_read += actual;
      }
      if (status != ZX_OK || (pending & ZX_SOCKET_PEER_CLOSED)) {
        break;
      }
    }
  });

  // From here on use EXPECT, not ASSERT, so that |consumer| is always joined.
  EXPECT_EQ(start_sent.wait_one(ZX_USER_SIGNAL_0, zx::time::infinite(), nullptr), ZX_OK);
  RunLoopUntilIdle();
  EXPECT_EQ(GetSessionState(), SessionState::kStarting);

  provider->SendOnSaveBuffer(/*wrapped_count=*/0, /*durable_data_end=*/16);
  RunLoopUntilIdle();
  EXPECT_EQ(provider->notify_buffer_saved_count(), 0);

  provider->MarkStarted();
  RunLoopUntilIdle();
  // A failed save would terminate the provider without acking it. (|terminate_count()| can't be
  // checked here: the consumer may already have closed the controller, which terminates it too.)
  EXPECT_EQ(provider->notify_buffer_saved_count(), 1);

  EXPECT_EQ(controller_closed.wait_one(ZX_USER_SIGNAL_0, zx::time::infinite(), nullptr), ZX_OK);
  RunLoopUntilIdle();
  EXPECT_EQ(GetSessionState(), SessionState::kTerminating);
  provider->MarkTerminated();
  RunLoopUntilIdle();
  EXPECT_EQ(GetSessionState(), SessionState::kNonexistent);

  consumer.join();
  EXPECT_TRUE(start_ok);
  EXPECT_GE(bytes_read, save_bytes);
}

}  // namespace test
}  // namespace tracing
