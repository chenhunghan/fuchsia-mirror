// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/syslog/cpp/macros.h>
#include <lib/trace-engine/fields.h>

#include <cerrno>
#include <cstdio>
#include <cstring>
#include <memory>
#include <optional>
#include <string>
#include <vector>

#include <gmock/gmock.h>
#include <trace-reader/reader.h>
#include <trace-reader/records.h>

#include "src/performance/trace_manager/tests/trace_manager_test.h"

namespace tracing {
namespace test {
namespace {

using ::testing::ElementsAre;

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

// Reads and decodes every record in the deferred-mode backing file /traces/trace.fxt into
// |out_records|. Callers wrap this in ASSERT_NO_FATAL_FAILURE.
void ReadRecordsFromDeferredTraceFile(std::vector<trace::Record>* out_records) {
  std::unique_ptr<FILE, decltype(&fclose)> file(fopen("/traces/trace.fxt", "rb"), &fclose);
  ASSERT_NE(file, nullptr) << "Failed to open /traces/trace.fxt: " << strerror(errno);

  std::vector<uint8_t> bytes;
  uint8_t chunk_buf[4096];
  size_t bytes_read = 0;
  while ((bytes_read = fread(chunk_buf, sizeof(uint8_t), sizeof(chunk_buf), file.get())) > 0) {
    bytes.insert(bytes.end(), chunk_buf, chunk_buf + bytes_read);
  }
  ASSERT_EQ(ferror(file.get()), 0) << "Failed to read /traces/trace.fxt";
  ASSERT_EQ(bytes.size() % sizeof(uint64_t), 0u);

  trace::TraceReader reader(
      [out_records](trace::Record record) { out_records->push_back(std::move(record)); },
      [](std::string_view error) { ADD_FAILURE() << "TraceReader error: " << error; });
  std::vector<uint64_t> words(trace::BytesToWords(bytes.size()));
  std::memcpy(words.data(), bytes.data(), bytes.size());
  trace::Chunk chunk(words.data(), words.size());
  EXPECT_TRUE(reader.ReadRecords(chunk));
  EXPECT_EQ(chunk.remaining_words(), 0u);
}

}  // namespace

using SessionState = TraceManagerTest::SessionState;

TEST_F(TraceManagerTest, RetryAfterFailedStop) {
  ConnectToProvisionerService();

  EXPECT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name));

  ASSERT_TRUE(InitializeSession());

  ASSERT_TRUE(StartSession());

  fuchsia_tracing_controller::StopOptions stop_options{GetDefaultStopOptions()};
  session_client()->StopTracing({{std::move(stop_options)}}).ThenExactlyOnce([](auto& result) {
    ASSERT_TRUE(result.is_error());
    ASSERT_TRUE(result.error_value().is_domain_error());
    ASSERT_EQ(result.error_value().domain_error(), fuchsia_tracing_controller::StopError::kAborted);
  });
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStopping);

  // Drop the socket before marking all providers stopped. This causes
  // an error while writing the buffer to the socket. Trace session should
  // abort and terminate.
  DropSocket();
  MarkAllProvidersStopped();
  RunLoopUntilIdle();
  // The session will already be gone because we dropped the socket.
  ASSERT_EQ(GetSessionState(), SessionState::kNonexistent);
  MarkAllProvidersTerminated();
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kNonexistent);

  // Initialize a new session and verify that everything works fine
  zx::socket our_socket, their_socket;
  zx_status_t status = zx::socket::create(0u, &our_socket, &their_socket);
  ASSERT_EQ(status, ZX_OK);

  fuchsia_tracing_controller::TraceConfig config{GetDefaultTraceConfig()};
  auto endpoints = fidl::Endpoints<fuchsia_tracing_controller::Session>::Create();
  session_client() = fidl::Client(std::move(endpoints.client), dispatcher(), this);
  auto initialize_result = provisioner_client()->InitializeTracing(
      {{std::move(endpoints.server), std::move(config), std::move(their_socket)}});
  ASSERT_TRUE(initialize_result.is_ok());

  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kInitialized);
  RunLoopUntilIdle();

  fuchsia_tracing_controller::StartOptions start_options{GetDefaultStartOptions()};
  bool start_completed = false;
  session_client()
      ->StartTracing({{std::move(start_options)}})
      .ThenExactlyOnce([this, &start_completed](auto& result) {
        start_completed = true;
        QuitLoop();
      });
  RunLoopUntilIdle();
  MarkAllProvidersStarted();
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStarted);

  ASSERT_TRUE(StopSession());

  ASSERT_TRUE(TerminateSession());
}

template <typename T>
void TryExtraStop(TraceManagerTest* fixture, const T& interface_ptr) {
  fuchsia_tracing_controller::StopOptions stop_options{fixture->GetDefaultStopOptions()};
  bool stop_completed = false;
  interface_ptr->StopTracing({{std::move(stop_options)}})
      .ThenExactlyOnce([fixture, &stop_completed](auto& result) {
        ASSERT_TRUE(result.is_error());
        ASSERT_TRUE(result.error_value().is_domain_error());
        ASSERT_EQ(result.error_value().domain_error(),
                  fuchsia_tracing_controller::StopError::kNotStarted);
        stop_completed = true;
        fixture->QuitLoop();
      });
  fixture->RunLoopUntilIdle();
  FX_LOGS(DEBUG) << "Loop done, expecting session still stopped";
  EXPECT_TRUE(stop_completed);
  EXPECT_EQ(fixture->GetSessionState(), SessionState::kStopped);
}

TEST_F(TraceManagerTest, ExtraStop) {
  ConnectToProvisionerService();

  EXPECT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name));

  ASSERT_TRUE(InitializeSession());

  ASSERT_TRUE(StartSession());

  ASSERT_TRUE(StopSession());

  // Now try stopping again.
  // This should complete with a NOT_STARTED error
  TryExtraStop(this, session_client());
}

TEST_F(TraceManagerTest, StopWhileStopping) {
  ConnectToProvisionerService();

  FakeProvider* provider;
  EXPECT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name, &provider));

  ASSERT_TRUE(InitializeSession());

  ASSERT_TRUE(StartSession());

  fuchsia_tracing_controller::StopOptions stop1_options{GetDefaultStopOptions()};
  session_client()->StopTracing({{std::move(stop1_options)}}).ThenExactlyOnce([](auto& result) {
    ASSERT_TRUE(result.is_ok());
  });
  RunLoopUntilIdle();
  // The loop will exit for the transition to kStopping.
  FX_LOGS(DEBUG) << "Loop done, expecting session stopping";
  EXPECT_EQ(GetSessionState(), SessionState::kStopping);

  // Now try another Stop while we're still in |kStopping|.
  // The provider doesn't advance state until we tell it to, so we should
  // still remain in |kStopping|.
  fuchsia_tracing_controller::StopOptions stop2_options{GetDefaultStopOptions()};
  bool stop_completed = false;
  session_client()
      ->StopTracing({{std::move(stop2_options)}})
      .ThenExactlyOnce([this, &stop_completed](auto& result) {
        ASSERT_TRUE(result.is_error());
        ASSERT_TRUE(result.error_value().is_domain_error());
        ASSERT_EQ(result.error_value().domain_error(),
                  fuchsia_tracing_controller::StopError::kNotStarted);
        stop_completed = true;
        QuitLoop();
      });
  RunLoopUntilIdle();
  FX_LOGS(DEBUG) << "Stop loop done";
  EXPECT_TRUE(stop_completed);
  EXPECT_TRUE(GetSessionState() == SessionState::kStopping);

  MarkAllProvidersStopped();
  RunLoopUntilIdle();
}

// Regression test for https://fxbug.dev/486261552 (V1 provider):
// When a provider times out during Stop, SessionStopTimeout must terminate and evict it from the
// session so that subsequent session termination does not stall for another 60s stop timeout and a
// new InitializeTracing request succeeds immediately.
TEST_F(TraceManagerTest, StopTimeoutEvictsUnresponsiveProviderAndAllowsImmediateTerminate) {
  ConnectToProvisionerService();

  FakeProvider* provider1 = nullptr;
  FakeProvider* provider2 = nullptr;
  ASSERT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name, &provider1));
  ASSERT_TRUE(AddFakeProvider(kProvider2Pid, kProvider2Name, &provider2));
  RunLoopUntilIdle();

  ASSERT_TRUE(InitializeSession());
  ASSERT_TRUE(StartSession());

  bool stop_completed = false;
  fuchsia_tracing_controller::StopResult stop_result;
  session_client()
      ->StopTracing({{GetDefaultStopOptions()}})
      .ThenExactlyOnce([&stop_completed, &stop_result](
                           fidl::Result<fuchsia_tracing_controller::Session::StopTracing>& result) {
        ASSERT_TRUE(result.is_ok());
        stop_result = std::move(result.value());
        stop_completed = true;
      });
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStopping);

  // provider1 stops normally; provider2 hangs and never acknowledges Stop (nor Terminate).
  provider1->MarkStopped();
  RunLoopUntilIdle();
  EXPECT_FALSE(stop_completed);
  EXPECT_EQ(GetSessionState(), SessionState::kStopping);

  RunLoopFor(zx::sec(60));
  ASSERT_TRUE(stop_completed);
  ASSERT_EQ(GetSessionState(), SessionState::kStopped);

  // The hung provider received Terminate, while the healthy provider remains stopped.
  EXPECT_EQ(provider1->terminate_count(), 0);
  EXPECT_EQ(provider2->terminate_count(), 1);

  // Healthy provider's stats and trace records were delivered; hung provider is excluded.
  ASSERT_TRUE(stop_result.provider_stats().has_value());
  ASSERT_EQ(stop_result.provider_stats()->size(), 1u);
  EXPECT_EQ(stop_result.provider_stats()->at(0).name(), kProvider1Name);
  EXPECT_EQ(stop_result.provider_stats()->at(0).pid(), kProvider1Pid);
  EXPECT_GT(stop_result.provider_stats()->at(0).non_durable_bytes_written().value_or(0), 0u);

  std::vector<trace::Record> records = ReadRecordsFromSocket();
  EXPECT_THAT(ProviderNamesInRecords(records), ElementsAre(kProvider1Name));

  // Disconnecting the controller must terminate the session immediately once healthy provider1
  // terminates, without stalling another 60s waiting on already-timed-out provider2.
  DisconnectFromControllerService();
  RunLoopUntilIdle();
  provider1->MarkTerminated();
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kNonexistent);

  // A new InitializeTracing request succeeds immediately because the previous session did not
  // stall in kTerminating (FakeProvider::Initialize drops the new FIFO while in kTerminating,
  // matching a provider whose engine has not yet terminated).
  zx::socket our_socket, their_socket;
  ASSERT_EQ(zx::socket::create(0u, &our_socket, &their_socket), ZX_OK);
  auto endpoints = fidl::Endpoints<fuchsia_tracing_controller::Session>::Create();
  session_client() = fidl::Client(std::move(endpoints.client), dispatcher(), this);
  auto init_res = provisioner_client()->InitializeTracing(
      {{std::move(endpoints.server), GetDefaultTraceConfig(), std::move(their_socket)}});
  ASSERT_TRUE(init_res.is_ok());
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kInitialized);

  // Eviction only removes provider2 from the stopped session; its registration stays with
  // TraceManager, so the new session adds it again and sends it Initialize.
  EXPECT_EQ(provider1->initialize_count(), 2);
  EXPECT_EQ(provider2->initialize_count(), 2);

  DisconnectFromControllerService();
  RunLoopUntilIdle();
  provider1->MarkTerminated();
  RunLoopUntilIdle();
  EXPECT_EQ(GetSessionState(), SessionState::kNonexistent);
}

// Regression test for https://fxbug.dev/486261552 (V2 provider):
// Same scenario with ProviderV2, also verifying clean teardown while two-way Stop() and
// Terminate() FIDL calls remain pending on the wedged provider.
TEST_F(TraceManagerTest, StopTimeoutEvictsUnresponsiveV2ProviderAndAllowsImmediateTerminate) {
  ConnectToProvisionerService();

  FakeProviderV2* provider1 = nullptr;
  FakeProviderV2* provider2 = nullptr;
  ASSERT_TRUE(AddFakeProviderV2(kProvider1Pid, kProvider1Name, &provider1));
  ASSERT_TRUE(AddFakeProviderV2(kProvider2Pid, kProvider2Name, &provider2));
  RunLoopUntilIdle();

  ASSERT_TRUE(InitializeSession());
  ASSERT_TRUE(StartSession());

  bool stop_completed = false;
  fuchsia_tracing_controller::StopResult stop_result;
  session_client()
      ->StopTracing({{GetDefaultStopOptions()}})
      .ThenExactlyOnce([&stop_completed, &stop_result](
                           fidl::Result<fuchsia_tracing_controller::Session::StopTracing>& result) {
        ASSERT_TRUE(result.is_ok());
        stop_result = std::move(result.value());
        stop_completed = true;
      });
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStopping);

  // provider1 stops normally; provider2 hangs and never replies to Stop() or Terminate().
  provider1->MarkStopped();
  RunLoopUntilIdle();
  EXPECT_FALSE(stop_completed);
  EXPECT_EQ(GetSessionState(), SessionState::kStopping);

  RunLoopFor(zx::sec(60));
  ASSERT_TRUE(stop_completed);
  ASSERT_EQ(GetSessionState(), SessionState::kStopped);

  EXPECT_EQ(provider1->terminate_count(), 0);
  EXPECT_EQ(provider2->terminate_count(), 1);

  ASSERT_TRUE(stop_result.provider_stats().has_value());
  ASSERT_EQ(stop_result.provider_stats()->size(), 1u);
  EXPECT_EQ(stop_result.provider_stats()->at(0).name(), kProvider1Name);
  EXPECT_EQ(stop_result.provider_stats()->at(0).pid(), kProvider1Pid);
  EXPECT_GT(stop_result.provider_stats()->at(0).non_durable_bytes_written().value_or(0), 0u);

  std::vector<trace::Record> records = ReadRecordsFromSocket();
  EXPECT_THAT(ProviderNamesInRecords(records), ElementsAre(kProvider1Name));

  // Disconnecting the controller terminates immediately once provider1 replies to Terminate().
  DisconnectFromControllerService();
  RunLoopUntilIdle();
  provider1->MarkTerminated();
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kNonexistent);

  // A new InitializeTracing request succeeds immediately because the previous session did not
  // stall in kTerminating.
  zx::socket our_socket, their_socket;
  ASSERT_EQ(zx::socket::create(0u, &our_socket, &their_socket), ZX_OK);
  auto endpoints = fidl::Endpoints<fuchsia_tracing_controller::Session>::Create();
  session_client() = fidl::Client(std::move(endpoints.client), dispatcher(), this);
  auto init_res = provisioner_client()->InitializeTracing(
      {{std::move(endpoints.server), GetDefaultTraceConfig(), std::move(their_socket)}});
  ASSERT_TRUE(init_res.is_ok());
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kInitialized);

  // Eviction only removes provider2 from the stopped session; its ProviderConnection stays
  // registered with TraceManager, so the new session adds it again and sends it Initialize. Like
  // TraceProviderImpl, the still-terminating provider2 has no engine session for it but stays
  // connected (FakeProviderV2::Initialize does not close the channel while kTerminating).
  EXPECT_EQ(provider1->initialize_count(), 2);
  EXPECT_EQ(provider2->initialize_count(), 2);

  DisconnectFromControllerService();
  RunLoopUntilIdle();
  // provider2 is a member of the new session: it receives the new session's Terminate and, having
  // no engine session, replies immediately.
  EXPECT_EQ(provider2->terminate_count(), 2);
  provider1->MarkTerminated();
  RunLoopUntilIdle();
  EXPECT_EQ(GetSessionState(), SessionState::kNonexistent);
}

// Verify that after SessionStopTimeout evicts a hung V1 provider, late FIFO packets (SAVE_BUFFER,
// ALERT, STOPPED) from the evicted provider are ignored and calling StartTracing again on the
// kStopped session succeeds cleanly with the remaining healthy provider.
TEST_F(TraceManagerTest, StopTimeoutIgnoresLateEventsAndAllowsRestart) {
  ConnectToProvisionerService();

  FakeProvider* provider1 = nullptr;
  FakeProvider* provider2 = nullptr;
  ASSERT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name, &provider1));
  ASSERT_TRUE(AddFakeProvider(kProvider2Pid, kProvider2Name, &provider2));
  RunLoopUntilIdle();

  auto config = GetDefaultTraceConfig();
  config.buffering_mode(fuchsia_tracing::BufferingMode::kStreaming);
  ASSERT_TRUE(InitializeSession(std::move(config)));
  ASSERT_TRUE(StartSession());

  bool stop_completed = false;
  session_client()
      ->StopTracing({{GetDefaultStopOptions()}})
      .ThenExactlyOnce(
          [&stop_completed](fidl::Result<fuchsia_tracing_controller::Session::StopTracing>& res) {
            ASSERT_TRUE(res.is_ok());
            stop_completed = true;
          });
  RunLoopUntilIdle();
  provider1->MarkStopped();
  RunLoopUntilIdle();

  RunLoopFor(zx::sec(60));
  ASSERT_TRUE(stop_completed);
  ASSERT_EQ(GetSessionState(), SessionState::kStopped);
  EXPECT_EQ(provider2->terminate_count(), 1);

  // Drain records written during Stop.
  EXPECT_THAT(ProviderNamesInRecords(ReadRecordsFromSocket()), ElementsAre(kProvider1Name));

  // Late SAVE_BUFFER and ALERT from evicted provider2 must be ignored.
  provider2->SendSaveBuffer(/*wrapped_count=*/0, /*durable_data_end=*/16);
  provider2->SendAlert("late-v1-alert");
  RunLoopUntilIdle();
  provider1->SendAlert("valid-v1-alert");
  RunLoopUntilIdle();

  EXPECT_TRUE(IsSocketEmpty());

  std::string received_alert;
  session_client()->WatchAlert().ThenExactlyOnce([&received_alert](auto& result) {
    ASSERT_TRUE(result.is_ok());
    received_alert = result.value().alert_name();
  });
  RunLoopUntilIdle();
  EXPECT_EQ(received_alert, "valid-v1-alert");

  // Restarting the kStopped session must succeed with provider1 without hitting FX_DCHECK on
  // evicted provider2.
  bool restart_completed = false;
  session_client()
      ->StartTracing({{GetDefaultStartOptions()}})
      .ThenExactlyOnce(
          [&restart_completed](
              fidl::Result<fuchsia_tracing_controller::Session::StartTracing>& result) {
            ASSERT_TRUE(result.is_ok());
            restart_completed = true;
          });
  RunLoopUntilIdle();
  EXPECT_EQ(provider1->start_count(), 2);
  EXPECT_EQ(provider2->start_count(), 1);

  provider1->MarkStarted();
  RunLoopUntilIdle();
  ASSERT_TRUE(restart_completed);
  ASSERT_EQ(GetSessionState(), SessionState::kStarted);

  // Even a super-late STOPPED packet from provider2 is ignored.
  provider2->SendStoppedPacket();
  RunLoopUntilIdle();
  EXPECT_EQ(GetSessionState(), SessionState::kStarted);
  EXPECT_TRUE(IsSocketEmpty());

  DisconnectFromControllerService();
  RunLoopUntilIdle();
  provider1->MarkTerminated();
  RunLoopUntilIdle();
  EXPECT_EQ(GetSessionState(), SessionState::kNonexistent);
}

// Verify that after SessionStopTimeout evicts a hung V2 provider, late FIDL events/replies
// (OnSaveBuffer, OnAlert, Stop reply) from the evicted provider are ignored and calling
// StartTracing again on the kStopped session succeeds cleanly with the remaining healthy provider.
TEST_F(TraceManagerTest, StopTimeoutIgnoresLateV2EventsAndAllowsRestart) {
  ConnectToProvisionerService();

  FakeProviderV2* provider1 = nullptr;
  FakeProviderV2* provider2 = nullptr;
  ASSERT_TRUE(AddFakeProviderV2(kProvider1Pid, kProvider1Name, &provider1));
  ASSERT_TRUE(AddFakeProviderV2(kProvider2Pid, kProvider2Name, &provider2));
  RunLoopUntilIdle();

  auto config = GetDefaultTraceConfig();
  config.buffering_mode(fuchsia_tracing::BufferingMode::kStreaming);
  ASSERT_TRUE(InitializeSession(std::move(config)));
  ASSERT_TRUE(StartSession());

  bool stop_completed = false;
  session_client()
      ->StopTracing({{GetDefaultStopOptions()}})
      .ThenExactlyOnce(
          [&stop_completed](fidl::Result<fuchsia_tracing_controller::Session::StopTracing>& res) {
            ASSERT_TRUE(res.is_ok());
            stop_completed = true;
          });
  RunLoopUntilIdle();
  provider1->MarkStopped();
  RunLoopUntilIdle();

  RunLoopFor(zx::sec(60));
  ASSERT_TRUE(stop_completed);
  ASSERT_EQ(GetSessionState(), SessionState::kStopped);
  EXPECT_EQ(provider2->terminate_count(), 1);

  // Drain records written during Stop.
  EXPECT_THAT(ProviderNamesInRecords(ReadRecordsFromSocket()), ElementsAre(kProvider1Name));

  // Late OnSaveBuffer and OnAlert from evicted provider2 must be ignored.
  provider2->SendOnSaveBuffer(/*wrapped_count=*/0, /*durable_data_end=*/16);
  provider2->SendOnAlert("late-v2-alert");
  RunLoopUntilIdle();
  provider1->SendOnAlert("valid-v2-alert");
  RunLoopUntilIdle();

  EXPECT_EQ(provider2->notify_buffer_saved_count(), 0);
  EXPECT_TRUE(IsSocketEmpty());

  std::string received_alert;
  session_client()->WatchAlert().ThenExactlyOnce([&received_alert](auto& result) {
    ASSERT_TRUE(result.is_ok());
    received_alert = result.value().alert_name();
  });
  RunLoopUntilIdle();
  EXPECT_EQ(received_alert, "valid-v2-alert");

  // Restarting the kStopped session must succeed with provider1 without hitting FX_DCHECK on
  // evicted provider2.
  bool restart_completed = false;
  session_client()
      ->StartTracing({{GetDefaultStartOptions()}})
      .ThenExactlyOnce(
          [&restart_completed](
              fidl::Result<fuchsia_tracing_controller::Session::StartTracing>& result) {
            ASSERT_TRUE(result.is_ok());
            restart_completed = true;
          });
  RunLoopUntilIdle();
  EXPECT_EQ(provider1->start_count(), 2);
  EXPECT_EQ(provider2->start_count(), 1);

  provider1->MarkStarted();
  RunLoopUntilIdle();
  ASSERT_TRUE(restart_completed);
  ASSERT_EQ(GetSessionState(), SessionState::kStarted);

  // Even a late Stop() reply from provider2 is ignored.
  provider2->SendLateStopReply();
  RunLoopUntilIdle();
  EXPECT_EQ(GetSessionState(), SessionState::kStarted);
  EXPECT_TRUE(IsSocketEmpty());

  DisconnectFromControllerService();
  RunLoopUntilIdle();
  provider1->MarkTerminated();
  RunLoopUntilIdle();
  EXPECT_EQ(GetSessionState(), SessionState::kNonexistent);
}

// Verify synchronous FinishStop() failure safely self-deletes TraceSession without UAF when
// triggered from StopTracing with additional_categories populated.
TEST_F(TraceManagerTest, StopFlushFailureWithAdditionalCategoriesAbortsWithoutUaf) {
  ConnectToProvisionerService();

  auto config = GetDefaultTraceConfig();
  config.defer_transfer(true);
  ASSERT_TRUE(InitializeSession(std::move(config)));

  auto start_options = GetDefaultStartOptions();
  start_options.additional_categories(std::vector<std::string>{
      "extra_category_alpha_long_string_for_heap_storage",
      "extra_category_beta_long_string_for_heap_storage",
  });
  ASSERT_TRUE(StartSession(std::move(start_options)));

  DropSocket();
  std::optional<fuchsia_tracing_controller::StopError> stop_error;
  session_client()
      ->StopTracing({{GetDefaultStopOptions()}})
      .ThenExactlyOnce(
          [&stop_error](fidl::Result<fuchsia_tracing_controller::Session::StopTracing>& res) {
            ASSERT_TRUE(res.is_error());
            ASSERT_TRUE(res.error_value().is_domain_error());
            stop_error = res.error_value().domain_error();
          });
  RunLoopUntilIdle();
  EXPECT_EQ(stop_error, fuchsia_tracing_controller::StopError::kAborted);
  EXPECT_EQ(GetSessionState(), SessionState::kNonexistent);
}

// Verify synchronous FinishStop() failure safely self-deletes TraceSession without UAF when a V1
// provider disconnects while the session is kStopping.
TEST_F(TraceManagerTest, StopFlushFailureOnV1ProviderDisconnectAbortsWithoutUaf) {
  ConnectToProvisionerService();

  FakeProvider* v1_provider = nullptr;
  ASSERT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name, &v1_provider));
  RunLoopUntilIdle();

  auto config = GetDefaultTraceConfig();
  config.defer_transfer(true);
  ASSERT_TRUE(InitializeSession(std::move(config)));
  ASSERT_TRUE(StartSession());

  std::optional<fuchsia_tracing_controller::StopError> stop_error;
  session_client()
      ->StopTracing({{GetDefaultStopOptions()}})
      .ThenExactlyOnce(
          [&stop_error](fidl::Result<fuchsia_tracing_controller::Session::StopTracing>& res) {
            ASSERT_TRUE(res.is_error());
            ASSERT_TRUE(res.error_value().is_domain_error());
            stop_error = res.error_value().domain_error();
          });
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStopping);

  DropSocket();
  DisconnectAllProviders();
  RunLoopUntilIdle();
  EXPECT_EQ(stop_error, fuchsia_tracing_controller::StopError::kAborted);
  EXPECT_EQ(GetSessionState(), SessionState::kNonexistent);
}

// Verify synchronous FinishStop() failure safely self-deletes TraceSession without UAF when a V2
// provider disconnects while the session is kStopping.
TEST_F(TraceManagerTest, StopFlushFailureOnV2ProviderDisconnectAbortsWithoutUaf) {
  ConnectToProvisionerService();

  FakeProviderV2* v2_provider = nullptr;
  ASSERT_TRUE(AddFakeProviderV2(kProvider1Pid, kProvider1Name, &v2_provider));
  RunLoopUntilIdle();

  auto config = GetDefaultTraceConfig();
  config.defer_transfer(true);
  ASSERT_TRUE(InitializeSession(std::move(config)));
  ASSERT_TRUE(StartSession());

  std::optional<fuchsia_tracing_controller::StopError> stop_error;
  session_client()
      ->StopTracing({{GetDefaultStopOptions()}})
      .ThenExactlyOnce(
          [&stop_error](fidl::Result<fuchsia_tracing_controller::Session::StopTracing>& res) {
            ASSERT_TRUE(res.is_error());
            ASSERT_TRUE(res.error_value().is_domain_error());
            stop_error = res.error_value().domain_error();
          });
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStopping);

  DropSocket();
  DisconnectAllProviders();
  RunLoopUntilIdle();
  EXPECT_EQ(stop_error, fuchsia_tracing_controller::StopError::kAborted);
  EXPECT_EQ(GetSessionState(), SessionState::kNonexistent);
}

// Verify that on a deferred session, a provider stopping normally writes its buffer to the backing
// file (which succeeds even though the consumer socket is closed), and FinishStop() then aborts the
// session with kAborted when flushing that file to the closed socket fails.
TEST_F(TraceManagerTest, DeferredStopFlushFailureAfterProviderStopsAborts) {
  ConnectToProvisionerService();

  ASSERT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name));
  RunLoopUntilIdle();

  auto config = GetDefaultTraceConfig();
  config.defer_transfer(true);
  ASSERT_TRUE(InitializeSession(std::move(config)));
  ASSERT_TRUE(StartSession());

  std::optional<fuchsia_tracing_controller::StopError> stop_error;
  session_client()
      ->StopTracing({{GetDefaultStopOptions()}})
      .ThenExactlyOnce(
          [&stop_error](fidl::Result<fuchsia_tracing_controller::Session::StopTracing>& res) {
            ASSERT_TRUE(res.is_error());
            ASSERT_TRUE(res.error_value().is_domain_error());
            stop_error = res.error_value().domain_error();
          });
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStopping);

  DropSocket();
  MarkAllProvidersStopped();
  RunLoopUntilIdle();
  EXPECT_EQ(stop_error, fuchsia_tracing_controller::StopError::kAborted);
  EXPECT_EQ(GetSessionState(), SessionState::kNonexistent);
}

// Verify that on a deferred session, SessionStopTimeout() evicting an unresponsive provider and
// then failing to flush the backing file to a closed consumer socket aborts the session with
// kAborted.
TEST_F(TraceManagerTest, DeferredStopFlushFailureAfterStopTimeoutAborts) {
  ConnectToProvisionerService();

  ASSERT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name));
  RunLoopUntilIdle();

  auto config = GetDefaultTraceConfig();
  config.defer_transfer(true);
  ASSERT_TRUE(InitializeSession(std::move(config)));
  ASSERT_TRUE(StartSession());

  std::optional<fuchsia_tracing_controller::StopError> stop_error;
  session_client()
      ->StopTracing({{GetDefaultStopOptions()}})
      .ThenExactlyOnce(
          [&stop_error](fidl::Result<fuchsia_tracing_controller::Session::StopTracing>& res) {
            ASSERT_TRUE(res.is_error());
            ASSERT_TRUE(res.error_value().is_domain_error());
            stop_error = res.error_value().domain_error();
          });
  RunLoopUntilIdle();
  ASSERT_EQ(GetSessionState(), SessionState::kStopping);

  // The provider never acks Stop, so the 60s stop timeout fires.
  DropSocket();
  RunLoopFor(zx::sec(60));
  EXPECT_EQ(stop_error, fuchsia_tracing_controller::StopError::kAborted);
  EXPECT_EQ(GetSessionState(), SessionState::kNonexistent);
}

// Verify that closing the consumer socket and then terminating a started deferred session without
// calling StopTracing still copies provider buffers into /traces/trace.fxt during termination, so
// harnesses that retrieve the file directly when the socket transfer fails
// (https://fxbug.dev/446873535) get the provider data.
TEST_F(TraceManagerTest, DeferredTerminateWithDroppedSocketWritesProviderDataToFile) {
  ConnectToProvisionerService();

  ASSERT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name));
  RunLoopUntilIdle();

  auto config = GetDefaultTraceConfig();
  config.defer_transfer(true);
  ASSERT_TRUE(InitializeSession(std::move(config)));
  ASSERT_TRUE(StartSession());

  DropSocket();
  ASSERT_TRUE(TerminateSession());
  EXPECT_EQ(GetSessionState(), SessionState::kNonexistent);

  std::vector<trace::Record> records;
  ASSERT_NO_FATAL_FAILURE(ReadRecordsFromDeferredTraceFile(&records));
  EXPECT_THAT(ProviderNamesInRecords(records), ElementsAre(kProvider1Name));
}

// Verify multi-run deferred sessions flush subsequent runs cleanly without dropping Run 2 records
// or re-sending Run 1 records.
TEST_F(TraceManagerTest, DeferredMultiRunFlushesSubsequentRunRecords) {
  ConnectToProvisionerService();

  FakeProvider* v1_provider = nullptr;
  ASSERT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name, &v1_provider));
  RunLoopUntilIdle();

  auto config = GetDefaultTraceConfig();
  config.defer_transfer(true);
  ASSERT_TRUE(InitializeSession(std::move(config)));

  std::vector<trace::Record> records;
  trace::TraceReader reader(
      [&records](trace::Record record) { records.push_back(std::move(record)); },
      [](std::string_view error) { ADD_FAILURE() << "TraceReader error: " << error; });

  auto drain_into_reader = [&]() {
    std::vector<uint8_t> bytes;
    uint8_t chunk_buf[4096];
    while (true) {
      size_t actual = 0;
      zx_status_t status = destination().read(0, chunk_buf, sizeof(chunk_buf), &actual);
      if (status == ZX_ERR_SHOULD_WAIT || status == ZX_ERR_PEER_CLOSED) {
        break;
      }
      ASSERT_EQ(status, ZX_OK);
      bytes.insert(bytes.end(), chunk_buf, chunk_buf + actual);
    }
    ASSERT_GT(bytes.size(), 0u);
    ASSERT_EQ(bytes.size() % sizeof(uint64_t), 0u);
    std::vector<uint64_t> words(trace::BytesToWords(bytes.size()));
    std::memcpy(words.data(), bytes.data(), bytes.size());
    trace::Chunk chunk(words.data(), words.size());
    EXPECT_TRUE(reader.ReadRecords(chunk));
    EXPECT_EQ(chunk.remaining_words(), 0u);
  };

  auto count_init_records = [&]() {
    return std::ranges::count_if(records, [](const trace::Record& r) {
      return r.type() == trace::RecordType::kInitialization;
    });
  };

  ASSERT_TRUE(StartSession());
  ASSERT_TRUE(StopSession());
  ASSERT_NO_FATAL_FAILURE(drain_into_reader());
  EXPECT_THAT(ProviderNamesInRecords(records), ElementsAre(kProvider1Name));
  EXPECT_EQ(count_init_records(), 1);

  ASSERT_TRUE(StartSession());
  ASSERT_TRUE(StopSession());
  ASSERT_NO_FATAL_FAILURE(drain_into_reader());
  // ProviderInfo was written only in Run 1 (not re-sent), while Run 2's Initialization record was
  // flushed cleanly.
  EXPECT_THAT(ProviderNamesInRecords(records), ElementsAre(kProvider1Name));
  EXPECT_EQ(count_init_records(), 2);

  ASSERT_TRUE(TerminateSession());
}

// Verify StopTracing with write_results=false on a deferred session does not flush
// /traces/trace.fxt to the consumer socket until a subsequent StopTracing(write_results=true) or
// session termination requests results.
TEST_F(TraceManagerTest, DeferredStopWithoutWriteResultsDoesNotFlushFileUntilRequested) {
  ConnectToProvisionerService();

  FakeProvider* v1_provider = nullptr;
  ASSERT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name, &v1_provider));
  RunLoopUntilIdle();

  auto config = GetDefaultTraceConfig();
  config.defer_transfer(true);
  ASSERT_TRUE(InitializeSession(std::move(config)));

  std::vector<trace::Record> records;
  trace::TraceReader reader(
      [&records](trace::Record record) { records.push_back(std::move(record)); },
      [](std::string_view error) { ADD_FAILURE() << "TraceReader error: " << error; });

  auto drain_into_reader = [&]() {
    std::vector<uint8_t> bytes;
    uint8_t chunk_buf[4096];
    while (true) {
      size_t actual = 0;
      zx_status_t status = destination().read(0, chunk_buf, sizeof(chunk_buf), &actual);
      if (status == ZX_ERR_SHOULD_WAIT || status == ZX_ERR_PEER_CLOSED) {
        break;
      }
      ASSERT_EQ(status, ZX_OK);
      bytes.insert(bytes.end(), chunk_buf, chunk_buf + actual);
    }
    ASSERT_GT(bytes.size(), 0u);
    ASSERT_EQ(bytes.size() % sizeof(uint64_t), 0u);
    std::vector<uint64_t> words(trace::BytesToWords(bytes.size()));
    std::memcpy(words.data(), bytes.data(), bytes.size());
    trace::Chunk chunk(words.data(), words.size());
    EXPECT_TRUE(reader.ReadRecords(chunk));
    EXPECT_EQ(chunk.remaining_words(), 0u);
  };

  auto count_init_records = [&]() {
    return std::ranges::count_if(records, [](const trace::Record& r) {
      return r.type() == trace::RecordType::kInitialization;
    });
  };

  // Run 1: StopTracing with write_results=false must NOT flush /traces/trace.fxt to destination().
  ASSERT_TRUE(StartSession());
  fuchsia_tracing_controller::StopOptions no_write_options;
  no_write_options.write_results(false);
  ASSERT_TRUE(StopSession(std::move(no_write_options)));

  zx_signals_t pending = 0;
  EXPECT_EQ(destination().wait_one(ZX_SOCKET_READABLE, zx::time::infinite_past(), &pending),
            ZX_ERR_TIMED_OUT);

  // Run 2: StopTracing with write_results=true flushes buffered trace data (Run 1 retained + Run
  // 2).
  ASSERT_TRUE(StartSession());
  ASSERT_TRUE(StopSession());
  ASSERT_NO_FATAL_FAILURE(drain_into_reader());
  EXPECT_THAT(ProviderNamesInRecords(records), ElementsAre(kProvider1Name));
  const auto init_count_after_run2 = count_init_records();
  EXPECT_GE(init_count_after_run2, 1);

  // Run 3: StopTracing with write_results=false again leaves destination() unreadable, while
  // subsequent TerminateSession() flushes the retained Run 3 records.
  ASSERT_TRUE(StartSession());
  fuchsia_tracing_controller::StopOptions no_write_options_2;
  no_write_options_2.write_results(false);
  ASSERT_TRUE(StopSession(std::move(no_write_options_2)));
  EXPECT_EQ(destination().wait_one(ZX_SOCKET_READABLE, zx::time::infinite_past(), &pending),
            ZX_ERR_TIMED_OUT);

  ASSERT_TRUE(TerminateSession());
  ASSERT_NO_FATAL_FAILURE(drain_into_reader());
  EXPECT_GT(count_init_records(), init_count_after_run2);
}

// Verify the `ffx trace stop --abort` sequence (`TraceTask::abort()`: drop the socket, then
// `StopTracing({ write_results: false })`) on a `defer_transfer = true` session returns Ok, and
// that the subsequent `TerminateSession()` still copies provider buffers into `/traces/trace.fxt`.
// Test harnesses rely on this when they retrieve the file directly after a failed socket transfer
// (https://fxbug.dev/446873535).
TEST_F(TraceManagerTest, DeferredStopWithoutWriteResultsAndDroppedSocketWritesFileOnTerminate) {
  ConnectToProvisionerService();

  FakeProvider* v1_provider = nullptr;
  ASSERT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name, &v1_provider));
  RunLoopUntilIdle();

  auto config = GetDefaultTraceConfig();
  config.defer_transfer(true);
  ASSERT_TRUE(InitializeSession(std::move(config)));
  ASSERT_TRUE(StartSession());

  DropSocket();
  fuchsia_tracing_controller::StopOptions no_write_options;
  no_write_options.write_results(false);
  ASSERT_TRUE(StopSession(std::move(no_write_options)));
  EXPECT_EQ(GetSessionState(), SessionState::kStopped);

  ASSERT_TRUE(TerminateSession());
  EXPECT_EQ(GetSessionState(), SessionState::kNonexistent);

  std::vector<trace::Record> records;
  ASSERT_NO_FATAL_FAILURE(ReadRecordsFromDeferredTraceFile(&records));
  EXPECT_THAT(ProviderNamesInRecords(records), ElementsAre(kProvider1Name));
}

namespace {

constexpr uint32_t kShortStartTimeoutMilliseconds = 1000;

// Pipelines StartTracing and StopTracing while the registered provider never acks start, and
// verifies StartTracing completes with Ok before StopTracing replies, and that the start timeout
// firing later neither reverts the stopped session to kStarted nor replies again.
void VerifyStopWhileStartingCompletesStartFirst(TraceManagerTest* fixture) {
  auto config = TraceManagerTest::GetDefaultTraceConfig();
  config.start_timeout_milliseconds(kShortStartTimeoutMilliseconds);
  ASSERT_TRUE(fixture->InitializeSession(std::move(config)));

  // Shared so that replies delivered during fixture teardown never touch a dead stack frame.
  struct Replies {
    std::vector<std::string> order;
    std::optional<bool> start_ok;
    std::optional<bool> stop_ok;
  };
  auto replies = std::make_shared<Replies>();
  fixture->session_client()
      ->StartTracing({{TraceManagerTest::GetDefaultStartOptions()}})
      .ThenExactlyOnce(
          [replies](fidl::Result<fuchsia_tracing_controller::Session::StartTracing>& result) {
            replies->order.emplace_back("start");
            replies->start_ok = result.is_ok();
          });
  fixture->session_client()
      ->StopTracing({{TraceManagerTest::GetDefaultStopOptions()}})
      .ThenExactlyOnce(
          [replies](fidl::Result<fuchsia_tracing_controller::Session::StopTracing>& result) {
            replies->order.emplace_back("stop");
            replies->stop_ok = result.is_ok();
          });
  fixture->RunLoopUntilIdle();
  EXPECT_EQ(fixture->GetSessionState(), SessionState::kStopping);
  EXPECT_THAT(replies->order, ElementsAre("start"));
  EXPECT_EQ(replies->start_ok, true);

  fixture->MarkAllProvidersStopped();
  fixture->RunLoopUntilIdle();
  EXPECT_EQ(fixture->GetSessionState(), SessionState::kStopped);
  EXPECT_THAT(replies->order, ElementsAre("start", "stop"));
  EXPECT_EQ(replies->stop_ok, true);

  // Advance past the start timeout: state must remain kStopped with no further replies.
  fixture->RunLoopFor(zx::msec(2 * kShortStartTimeoutMilliseconds));
  EXPECT_EQ(fixture->GetSessionState(), SessionState::kStopped);
  EXPECT_THAT(replies->order, ElementsAre("start", "stop"));

  EXPECT_TRUE(fixture->TerminateSession());
}

}  // namespace

// Verify StopTracing while a V1 provider has not acked start completes StartTracing with Ok before
// StopTracing, and cancels session_start_timeout_ so it cannot revert the session to kStarted.
TEST_F(TraceManagerTest, StopWhileStartingCompletesStartBeforeStopV1) {
  ConnectToProvisionerService();
  ASSERT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name));
  RunLoopUntilIdle();
  VerifyStopWhileStartingCompletesStartFirst(this);
}

// Same as above for a V2 provider.
TEST_F(TraceManagerTest, StopWhileStartingCompletesStartBeforeStopV2) {
  ConnectToProvisionerService();
  ASSERT_TRUE(AddFakeProviderV2(kProvider1Pid, kProvider1Name));
  RunLoopUntilIdle();
  VerifyStopWhileStartingCompletesStartFirst(this);
}

namespace {

// Closes the session channel while the registered provider has not acked start, and verifies the
// start timeout firing during kTerminating does not revert the session to kStarted, and that
// termination then completes cleanly.
void VerifyTerminateWhileStartingTerminatesCleanly(TraceManagerTest* fixture) {
  auto config = TraceManagerTest::GetDefaultTraceConfig();
  config.start_timeout_milliseconds(kShortStartTimeoutMilliseconds);
  ASSERT_TRUE(fixture->InitializeSession(std::move(config)));
  fixture->BeginStartSession();
  ASSERT_EQ(fixture->GetSessionState(), SessionState::kStarting);

  fixture->BeginTerminateSession();
  ASSERT_EQ(fixture->GetSessionState(), SessionState::kTerminating);

  // Advance past the start timeout while the provider has not yet acked Terminate: the session
  // must remain kTerminating.
  fixture->RunLoopFor(zx::msec(2 * kShortStartTimeoutMilliseconds));
  EXPECT_EQ(fixture->GetSessionState(), SessionState::kTerminating);

  ASSERT_TRUE(fixture->FinishTerminateSession());
  EXPECT_EQ(fixture->GetSessionState(), SessionState::kNonexistent);
}

}  // namespace

// Verify closing the session channel while a V1 provider has not acked start terminates cleanly,
// and the start timeout does not revert a terminating session to kStarted.
TEST_F(TraceManagerTest, TerminateWhileStartingTerminatesCleanlyV1) {
  ConnectToProvisionerService();
  ASSERT_TRUE(AddFakeProvider(kProvider1Pid, kProvider1Name));
  RunLoopUntilIdle();
  VerifyTerminateWhileStartingTerminatesCleanly(this);
}

// Same as above for a V2 provider.
TEST_F(TraceManagerTest, TerminateWhileStartingTerminatesCleanlyV2) {
  ConnectToProvisionerService();
  ASSERT_TRUE(AddFakeProviderV2(kProvider1Pid, kProvider1Name));
  RunLoopUntilIdle();
  VerifyTerminateWhileStartingTerminatesCleanly(this);
}

}  // namespace test
}  // namespace tracing
