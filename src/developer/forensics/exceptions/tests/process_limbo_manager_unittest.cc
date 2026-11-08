// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#include "src/developer/forensics/exceptions/process_limbo_manager.h"

#include <fidl/fuchsia.exception/cpp/fidl.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/syslog/cpp/macros.h>

#include <optional>
#include <string>
#include <vector>

#include <gtest/gtest.h>

#include "src/developer/forensics/exceptions/exception_broker.h"
#include "src/developer/forensics/exceptions/tests/crasher_wrapper.h"
#include "src/developer/forensics/testing/unit_test_fixture.h"
#include "src/lib/fsl/handles/object_info.h"

namespace forensics {
namespace exceptions {
namespace {

using fuchsia_exception::ExceptionInfo;
using fuchsia_exception::ExceptionType;
using fuchsia_exception::ProcessException;
using fuchsia_exception::ProcessExceptionInfo;
using fuchsia_exception::ProcessExceptionMetadata;
using fuchsia_exception::ProcessLimbo;

using GetActiveResult = fidl::Result<ProcessLimbo::GetActive>;
using GetFiltersResult = fidl::Result<ProcessLimbo::GetFilters>;
using ListProcessesResult = fidl::Result<ProcessLimbo::ListProcessesWaitingOnException>;
using RetrieveExceptionResult = fidl::Result<ProcessLimbo::RetrieveException>;
using WatchActiveResult = fidl::Result<ProcessLimbo::WatchActive>;
using WatchProcessesResult = fidl::Result<ProcessLimbo::WatchProcessesWaitingOnException>;

// Returns the domain error of |result|, or std::nullopt if the call succeeded or failed with a
// framework error.
template <typename T>
std::optional<zx_status_t> DomainError(const T& result) {
  if (result.is_ok() || !result.error_value().is_domain_error()) {
    return std::nullopt;
  }
  return result.error_value().domain_error();
}

bool RetrieveExceptionContext(ExceptionContext* pe) {
  // Create a process that crashes and obtain the relevant handles and exception.
  // By the time |SpawnCrasher| has returned, the process has already thrown an exception.
  if (!SpawnCrasher(pe)) {
    FX_LOGS(ERROR) << "Could not spawn crasher process.";
    return false;
  }

  // We mark the exception to be handled. We need this because we pass on the exception to the
  // handler, which will resume it before we get the control back. If we don't mark it as handled,
  // the exception will bubble out of our environment.
  return MarkExceptionAsHandled(pe);
}

ExceptionInfo ExceptionContextToExceptionInfo(const ExceptionContext& pe) {
  // Translate the exception to the fidl format.
  ExceptionInfo exception_info;
  exception_info.process_koid(pe.exception_info.pid);
  exception_info.thread_koid(pe.exception_info.tid);
  exception_info.type(static_cast<ExceptionType>(pe.exception_info.type));

  return exception_info;
}

void AddExceptionToLimbo(ProcessLimboManager* limbo_manager, zx::exception exception,
                         ExceptionInfo info) {
  ProcessException process_exception = {};
  process_exception.exception(std::move(exception));
  process_exception.info(std::move(info));

  zx_status_t status;
  zx::process process;
  status = process_exception.exception()->get_process(&process);
  if (status != ZX_OK) {
    FX_PLOGS(WARNING, status) << "Could not obtain process handle for exception.";
  } else {
    process_exception.process(std::move(process));
  }

  zx::thread thread;
  status = process_exception.exception()->get_thread(&thread);
  if (status != ZX_OK) {
    FX_PLOGS(WARNING, status) << "Could not obtain thread handle for exception.";
  } else {
    process_exception.thread(std::move(thread));
  }

  limbo_manager->AddToLimbo(std::move(process_exception));
}

bool ValidateException(const ProcessExceptionMetadata&) { return true; }
bool ValidateException(const ProcessException& exception) {
  return exception.exception().has_value();
}

template <typename T>
void ValidateException(const ExceptionContext& context, const T& process_exception) {
  if (std::is_same<T, ProcessException>::value)
    ASSERT_TRUE(ValidateException(process_exception));
  ASSERT_TRUE(process_exception.info().has_value());
  ASSERT_TRUE(process_exception.process().has_value());
  ASSERT_TRUE(process_exception.thread().has_value());

  const zx::process& process = process_exception.process().value();
  ASSERT_EQ(context.process_koid, fsl::GetKoid(process.get()));
  ASSERT_EQ(context.process_koid, process_exception.info()->process_koid());
  ASSERT_EQ(context.process_name, fsl::GetObjectName(process.get()));

  const zx::thread& thread = process_exception.thread().value();
  ASSERT_EQ(context.thread_koid, fsl::GetKoid(thread.get()));
  ASSERT_EQ(context.thread_koid, process_exception.info()->thread_koid());
  ASSERT_EQ(context.thread_name, fsl::GetObjectName(thread.get()));

  ASSERT_EQ(process_exception.info()->type(), ExceptionType::kFatalPageFault);
}

ProcessException GetFakeException(zx_koid_t process_koid = 1, zx_koid_t thread_koid = 1,
                                  ExceptionType type = ExceptionType::kFatalPageFault) {
  ExceptionInfo info;
  info.process_koid(process_koid);
  info.thread_koid(thread_koid);
  info.type(type);

  ProcessException exception;
  exception.info(info);
  exception.process(
      zx::process(static_cast<zx_handle_t>(process_koid)));  // Invalid handle. Never rely on it.
  exception.thread(
      zx::thread(static_cast<zx_handle_t>(thread_koid)));  // Invalid handle. Never rely on it.

  return exception;
}

// Tests -------------------------------------------------------------------------------------------

class ProcessLimboManagerTest : public UnitTestFixture {
 protected:
  // Creates a ProcessLimboHandler for |limbo_manager|, binds it to a new channel and returns a
  // client connected to it. The binding owns the handler.
  fidl::Client<ProcessLimbo> Connect(ProcessLimboManager* limbo_manager) {
    auto endpoints = fidl::CreateEndpoints<ProcessLimbo>();
    FX_CHECK(endpoints.is_ok()) << endpoints.status_string();

    auto handler = std::make_unique<ProcessLimboHandler>(limbo_manager->GetWeakPtr());
    limbo_manager->AddHandler(handler->GetWeakPtr());
    fidl::BindServer(dispatcher(), std::move(endpoints->server), std::move(handler));

    return fidl::Client<ProcessLimbo>(std::move(endpoints->client), dispatcher());
  }
};

TEST_F(ProcessLimboManagerTest, ProcessLimboHandler) {
  ProcessLimboManager limbo_manager;

  // Use the handler interface.
  fidl::Client<ProcessLimbo> client = Connect(&limbo_manager);

  // A disabled limbo should return an error.
  {
    std::optional<WatchProcessesResult> result;
    client->WatchProcessesWaitingOnException().Then(
        [&result](WatchProcessesResult& res) { result = std::move(res); });
    RunLoopUntilIdle();

    ASSERT_TRUE(result.has_value());
    EXPECT_EQ(DomainError(*result), ZX_ERR_UNAVAILABLE);
  }

  limbo_manager.SetActive(true);

  // We create multiple exceptions.
  ExceptionContext excps[3];
  ASSERT_TRUE(RetrieveExceptionContext(excps + 0));
  ASSERT_TRUE(RetrieveExceptionContext(excps + 1));
  ASSERT_TRUE(RetrieveExceptionContext(excps + 2));

  // Get the fidl representation of the exception.
  ExceptionInfo infos[3];
  infos[0] = ExceptionContextToExceptionInfo(excps[0]);
  infos[1] = ExceptionContextToExceptionInfo(excps[1]);
  infos[2] = ExceptionContextToExceptionInfo(excps[2]);

  AddExceptionToLimbo(&limbo_manager, std::move(excps[0].exception), infos[0]);
  AddExceptionToLimbo(&limbo_manager, std::move(excps[1].exception), infos[1]);
  AddExceptionToLimbo(&limbo_manager, std::move(excps[2].exception), infos[2]);

  {
    // There should be exceptions listed.
    std::optional<WatchProcessesResult> result;
    client->WatchProcessesWaitingOnException().Then(
        [&result](WatchProcessesResult& res) { result = std::move(res); });
    RunLoopUntilIdle();

    ASSERT_TRUE(result.has_value());
    ASSERT_TRUE(result->is_ok()) << result->error_value();
    const std::vector<ProcessExceptionMetadata>& exceptions = result->value().exception_list();
    ASSERT_EQ(exceptions.size(), 3u);
    ValidateException(excps[0], exceptions[0]);
    ValidateException(excps[1], exceptions[1]);
    ValidateException(excps[2], exceptions[2]);
  }

  {
    // ListProcessesWaitingOnException should succeed.
    std::optional<ListProcessesResult> result;
    client->ListProcessesWaitingOnException().Then(
        [&result](ListProcessesResult& res) { result = std::move(res); });
    RunLoopUntilIdle();

    ASSERT_TRUE(result.has_value());
    ASSERT_TRUE(result->is_ok()) << result->error_value();
    const std::vector<ProcessExceptionInfo>& exceptions = result->value().exception_list();
    ASSERT_EQ(exceptions.size(), 3u);
    ASSERT_EQ(excps[0].process_name, exceptions[0].process_name());
    ASSERT_EQ(excps[1].process_name, exceptions[1].process_name());
    ASSERT_EQ(excps[2].process_name, exceptions[2].process_name());
  }

  {
    // Getting a exception for a process that doesn't exist should fail.
    std::optional<RetrieveExceptionResult> result;
    client->RetrieveException({static_cast<zx_koid_t>(-1)})
        .Then([&result](RetrieveExceptionResult& res) { result = std::move(res); });
    RunLoopUntilIdle();

    ASSERT_TRUE(result.has_value());
    EXPECT_EQ(DomainError(*result), ZX_ERR_NOT_FOUND);

    // There should still be 3 exceptions.
    ASSERT_EQ(limbo_manager.limbo().size(), 3u);
  }

  {
    // Watching for the exception should not call.
    std::optional<WatchProcessesResult> watch_result;
    client->WatchProcessesWaitingOnException().Then(
        [&watch_result](WatchProcessesResult& res) { watch_result = std::move(res); });
    RunLoopUntilIdle();
    ASSERT_FALSE(watch_result.has_value());

    // Getting an actual exception should work.
    std::optional<RetrieveExceptionResult> result;
    client->RetrieveException({infos[0].process_koid()})
        .Then([&result](RetrieveExceptionResult& res) { result = std::move(res); });
    RunLoopUntilIdle();

    ASSERT_TRUE(result.has_value());
    ASSERT_TRUE(result->is_ok()) << result->error_value();
    ValidateException(excps[0], result->value().process_exception());

    // There should be one less exception.
    ASSERT_EQ(limbo_manager.limbo().size(), 2u);

    // We should've received a notification for the watch.
    ASSERT_TRUE(watch_result.has_value());
    ASSERT_TRUE(watch_result->is_ok()) << watch_result->error_value();
    const std::vector<ProcessExceptionMetadata>& exceptions =
        watch_result->value().exception_list();
    ASSERT_EQ(exceptions.size(), 2u);
    ValidateException(excps[1], exceptions[0]);
    ValidateException(excps[2], exceptions[1]);
  }

  {
    // That process should have been removed.
    std::optional<RetrieveExceptionResult> result;
    client->RetrieveException({infos[0].process_koid()})
        .Then([&result](RetrieveExceptionResult& res) { result = std::move(res); });
    RunLoopUntilIdle();

    ASSERT_TRUE(result.has_value());
    EXPECT_EQ(DomainError(*result), ZX_ERR_NOT_FOUND);
  }

  {
    // Watching for the exception should not call.
    std::optional<WatchProcessesResult> watch_result;
    client->WatchProcessesWaitingOnException().Then(
        [&watch_result](WatchProcessesResult& res) { watch_result = std::move(res); });
    RunLoopUntilIdle();
    ASSERT_FALSE(watch_result.has_value());

    // Asking for the other process should work.
    std::optional<RetrieveExceptionResult> result;
    client->RetrieveException({infos[2].process_koid()})
        .Then([&result](RetrieveExceptionResult& res) { result = std::move(res); });
    RunLoopUntilIdle();

    ASSERT_TRUE(result.has_value());
    ASSERT_TRUE(result->is_ok()) << result->error_value();
    ValidateException(excps[2], result->value().process_exception());

    // There should be one less exception.
    ASSERT_EQ(limbo_manager.limbo().size(), 1u);

    // We should've received a notification for the watch.
    ASSERT_TRUE(watch_result.has_value());
    ASSERT_TRUE(watch_result->is_ok()) << watch_result->error_value();
    const std::vector<ProcessExceptionMetadata>& exceptions =
        watch_result->value().exception_list();
    ASSERT_EQ(exceptions.size(), 1u);
    ValidateException(excps[1], exceptions[0]);
  }

  {
    // Watching for the exception should not call.
    std::optional<WatchProcessesResult> watch_result;
    client->WatchProcessesWaitingOnException().Then(
        [&watch_result](WatchProcessesResult& res) { watch_result = std::move(res); });
    RunLoopUntilIdle();
    ASSERT_FALSE(watch_result.has_value());

    // Getting the last one should work.
    std::optional<fidl::Result<ProcessLimbo::ReleaseProcess>> release_result;
    client->ReleaseProcess({infos[1].process_koid()})
        .Then([&release_result](fidl::Result<ProcessLimbo::ReleaseProcess>& res) {
          release_result = std::move(res);
        });
    RunLoopUntilIdle();

    ASSERT_TRUE(release_result.has_value());
    ASSERT_TRUE(release_result->is_ok()) << release_result->error_value();

    // There should be one less exception.
    ASSERT_EQ(limbo_manager.limbo().size(), 0u);

    // We should've received a notification for the watch.
    ASSERT_TRUE(watch_result.has_value());
    ASSERT_TRUE(watch_result->is_ok()) << watch_result->error_value();
    ASSERT_EQ(watch_result->value().exception_list().size(), 0u);
  }

  {
    // Releasing a process that isn't in the limbo should fail.
    std::optional<fidl::Result<ProcessLimbo::ReleaseProcess>> release_result;
    client->ReleaseProcess({infos[1].process_koid()})
        .Then([&release_result](fidl::Result<ProcessLimbo::ReleaseProcess>& res) {
          release_result = std::move(res);
        });
    RunLoopUntilIdle();

    ASSERT_TRUE(release_result.has_value());
    EXPECT_EQ(DomainError(*release_result), ZX_ERR_NOT_FOUND);
  }

  // We kill the jobs. This kills the underlying process. We do this so that the crashed process
  // doesn't get rescheduled. Otherwise the exception on the crash program would bubble out of our
  // environment and create noise on the overall system.
  excps[0].job.kill();
  excps[1].job.kill();
  excps[2].job.kill();
}

TEST_F(ProcessLimboManagerTest, FromExceptionBroker) {
  inspect::Inspector inspector;
  std::unique_ptr<ExceptionBroker> broker =
      ExceptionBroker::Create(dispatcher(), &inspector.GetRoot(), /*max_num_handlers=*/1u,
                              /*exception_ttl=*/zx::hour(1), /*suspend_enabled=*/false);
  ASSERT_TRUE(broker);
  ASSERT_TRUE(broker->limbo_manager().SetActive(true));

  // We create multiple exceptions.
  ExceptionContext excps[3];
  ASSERT_TRUE(RetrieveExceptionContext(excps + 0));
  ASSERT_TRUE(RetrieveExceptionContext(excps + 1));
  ASSERT_TRUE(RetrieveExceptionContext(excps + 2));

  // Get the fidl representation of the exception.
  ExceptionInfo infos[3];
  infos[0] = ExceptionContextToExceptionInfo(excps[0]);
  infos[1] = ExceptionContextToExceptionInfo(excps[1]);
  infos[2] = ExceptionContextToExceptionInfo(excps[2]);

  auto endpoints = fidl::CreateEndpoints<fuchsia_exception::Handler>();
  ASSERT_TRUE(endpoints.is_ok());

  fidl::ServerBinding<fuchsia_exception::Handler> binding(
      dispatcher(), std::move(endpoints->server), broker.get(), fidl::kIgnoreBindingClosure);
  fidl::Client<fuchsia_exception::Handler> client(std::move(endpoints->client), dispatcher());

  // It's not easy to pass array references to lambdas.
  bool cb_call0 = false;
  bool cb_call1 = false;
  bool cb_call2 = false;
  client->OnException({std::move(excps[0].exception), infos[0]})
      .ThenExactlyOnce([&cb_call0](fidl::Result<fuchsia_exception::Handler::OnException>& result) {
        EXPECT_TRUE(result.is_ok()) << result.error_value();
        cb_call0 = true;
      });
  client->OnException({std::move(excps[1].exception), infos[1]})
      .ThenExactlyOnce([&cb_call1](fidl::Result<fuchsia_exception::Handler::OnException>& result) {
        EXPECT_TRUE(result.is_ok()) << result.error_value();
        cb_call1 = true;
      });
  client->OnException({std::move(excps[2].exception), infos[2]})
      .ThenExactlyOnce([&cb_call2](fidl::Result<fuchsia_exception::Handler::OnException>& result) {
        EXPECT_TRUE(result.is_ok()) << result.error_value();
        cb_call2 = true;
      });

  RunLoopUntilIdle();
  EXPECT_TRUE(cb_call0);
  EXPECT_TRUE(cb_call1);
  EXPECT_TRUE(cb_call2);

  // There should be 3 exceptions on the limbo.
  const std::map<zx_koid_t, ProcessException>& limbo = broker->limbo_manager().limbo();
  ValidateException(excps[0], limbo.find(excps[0].process_koid)->second);
  ValidateException(excps[1], limbo.find(excps[1].process_koid)->second);
  ValidateException(excps[2], limbo.find(excps[2].process_koid)->second);

  // We kill the jobs. This kills the underlying process. We do this so that the crashed process
  // doesn't get rescheduled. Otherwise the exception on the crash program would bubble out of our
  // environment and create noise on the overall system.
  excps[0].job.kill();
  excps[1].job.kill();
  excps[2].job.kill();
}

TEST_F(ProcessLimboManagerTest, ExceptionsWithoutHandlesAreNotListed) {
  ProcessLimboManager limbo_manager;
  fidl::Client<ProcessLimbo> client = Connect(&limbo_manager);

  ASSERT_TRUE(limbo_manager.SetActive(true));

  // An exception whose process and thread handles couldn't be obtained.
  ExceptionInfo info;
  info.process_koid(1);
  info.thread_koid(1);
  info.type(ExceptionType::kFatalPageFault);

  ProcessException exception;
  exception.info(info);
  limbo_manager.AddToLimbo(std::move(exception));
  ASSERT_EQ(limbo_manager.limbo().size(), 1u);

  {
    std::optional<WatchProcessesResult> result;
    client->WatchProcessesWaitingOnException().Then(
        [&result](WatchProcessesResult& res) { result = std::move(res); });
    RunLoopUntilIdle();

    ASSERT_TRUE(result.has_value());
    ASSERT_TRUE(result->is_ok()) << result->error_value();
    EXPECT_TRUE(result->value().exception_list().empty());
  }

  {
    std::optional<ListProcessesResult> result;
    client->ListProcessesWaitingOnException().Then(
        [&result](ListProcessesResult& res) { result = std::move(res); });
    RunLoopUntilIdle();

    ASSERT_TRUE(result.has_value());
    ASSERT_TRUE(result->is_ok()) << result->error_value();
    EXPECT_TRUE(result->value().exception_list().empty());
  }
}

// WatchActive -------------------------------------------------------------------------------------

TEST_F(ProcessLimboManagerTest, WatchActiveCalls) {
  ProcessLimboManager limbo_manager;
  fidl::Client<ProcessLimbo> client = Connect(&limbo_manager);

  // As no hanging get has been made there should be no change.
  ASSERT_TRUE(limbo_manager.SetActive(true));

  // Making a get should return immediatelly.
  std::optional<WatchActiveResult> result;
  client->WatchActive().Then([&result](WatchActiveResult& res) { result = std::move(res); });
  RunLoopUntilIdle();

  ASSERT_TRUE(result.has_value());
  ASSERT_TRUE(result->is_ok()) << result->error_value();
  EXPECT_TRUE(result->value().is_active());

  // A second change should not trigger an event (hanging get).
  result = std::nullopt;
  client->WatchActive().Then([&result](WatchActiveResult& res) { result = std::move(res); });
  RunLoopUntilIdle();

  ASSERT_FALSE(result.has_value());

  // GetActive should always return.
  {
    std::optional<GetActiveResult> get_result;
    client->GetActive().Then([&get_result](GetActiveResult& res) { get_result = std::move(res); });
    RunLoopUntilIdle();

    ASSERT_TRUE(get_result.has_value());
    ASSERT_TRUE(get_result->is_ok()) << get_result->error_value();
    EXPECT_TRUE(get_result->value().is_active());
  }

  // Not making the state should no issue the call.
  ASSERT_FALSE(limbo_manager.SetActive(true));
  RunLoopUntilIdle();
  ASSERT_FALSE(result.has_value());

  // Changing the state should trigger the callback.
  ASSERT_TRUE(limbo_manager.SetActive(false));
  RunLoopUntilIdle();

  ASSERT_TRUE(result.has_value());
  ASSERT_TRUE(result->is_ok()) << result->error_value();
  EXPECT_FALSE(result->value().is_active());

  // A new get should wait for the next change.
  result = std::nullopt;
  client->WatchActive().Then([&result](WatchActiveResult& res) { result = std::move(res); });
  RunLoopUntilIdle();

  ASSERT_FALSE(result.has_value());

  ASSERT_TRUE(limbo_manager.SetActive(true));
  RunLoopUntilIdle();

  ASSERT_TRUE(result.has_value());
  ASSERT_TRUE(result->is_ok()) << result->error_value();
  EXPECT_TRUE(result->value().is_active());

  // Having an outstanding watch limbo call should fail when the limbo is disabled.

  {
    // The first call should return successfully an empty list.
    std::optional<WatchProcessesResult> watch_result;
    client->WatchProcessesWaitingOnException().Then(
        [&watch_result](WatchProcessesResult& res) { watch_result = std::move(res); });
    RunLoopUntilIdle();

    ASSERT_TRUE(watch_result.has_value());
    ASSERT_TRUE(watch_result->is_ok()) << watch_result->error_value();
    ASSERT_EQ(watch_result->value().exception_list().size(), 0u);
  }

  {
    // The second call should be pending.
    std::optional<WatchProcessesResult> watch_result;
    client->WatchProcessesWaitingOnException().Then(
        [&watch_result](WatchProcessesResult& res) { watch_result = std::move(res); });
    RunLoopUntilIdle();

    ASSERT_FALSE(watch_result.has_value());

    // Disabling the limbo should call the callback.
    ASSERT_TRUE(limbo_manager.SetActive(false));
    RunLoopUntilIdle();

    ASSERT_TRUE(watch_result.has_value());
    EXPECT_EQ(DomainError(*watch_result), ZX_ERR_CANCELED);
  }
}

TEST_F(ProcessLimboManagerTest, ConcurrentWatchActiveClosesConnection) {
  ProcessLimboManager limbo_manager;
  fidl::Client<ProcessLimbo> client = Connect(&limbo_manager);

  // The first call returns immediately.
  {
    std::optional<WatchActiveResult> result;
    client->WatchActive().Then([&result](WatchActiveResult& res) { result = std::move(res); });
    RunLoopUntilIdle();
    ASSERT_TRUE(result.has_value());
    ASSERT_TRUE(result->is_ok()) << result->error_value();
  }

  // Making two get calls should close the connection.
  std::optional<WatchActiveResult> result1;
  client->WatchActive().Then([&result1](WatchActiveResult& res) { result1 = std::move(res); });
  std::optional<WatchActiveResult> result2;
  client->WatchActive().Then([&result2](WatchActiveResult& res) { result2 = std::move(res); });
  RunLoopUntilIdle();

  ASSERT_TRUE(result1.has_value());
  ASSERT_TRUE(result1->is_error());
  EXPECT_TRUE(result1->error_value().is_peer_closed()) << result1->error_value();
  EXPECT_EQ(result1->error_value().status(), ZX_ERR_BAD_STATE) << result1->error_value();

  ASSERT_TRUE(result2.has_value());
  ASSERT_TRUE(result2->is_error());
  EXPECT_TRUE(result2->error_value().is_peer_closed()) << result2->error_value();
  EXPECT_EQ(result2->error_value().status(), ZX_ERR_BAD_STATE) << result2->error_value();
}

TEST_F(ProcessLimboManagerTest, ConcurrentWatchProcessesWaitingOnExceptionClosesConnection) {
  ProcessLimboManager limbo_manager;
  fidl::Client<ProcessLimbo> client = Connect(&limbo_manager);

  ASSERT_TRUE(limbo_manager.SetActive(true));

  // The first call returns immediately.
  {
    std::optional<WatchProcessesResult> result;
    client->WatchProcessesWaitingOnException().Then(
        [&result](WatchProcessesResult& res) { result = std::move(res); });
    RunLoopUntilIdle();
    ASSERT_TRUE(result.has_value());
    ASSERT_TRUE(result->is_ok()) << result->error_value();
  }

  // Making two get calls should close the connection.
  std::optional<WatchProcessesResult> result1;
  client->WatchProcessesWaitingOnException().Then(
      [&result1](WatchProcessesResult& res) { result1 = std::move(res); });
  std::optional<WatchProcessesResult> result2;
  client->WatchProcessesWaitingOnException().Then(
      [&result2](WatchProcessesResult& res) { result2 = std::move(res); });
  RunLoopUntilIdle();

  ASSERT_TRUE(result1.has_value());
  ASSERT_TRUE(result1->is_error());
  ASSERT_TRUE(result1->error_value().is_framework_error()) << result1->error_value();
  EXPECT_TRUE(result1->error_value().framework_error().is_peer_closed()) << result1->error_value();
  EXPECT_EQ(result1->error_value().framework_error().status(), ZX_ERR_BAD_STATE)
      << result1->error_value();

  ASSERT_TRUE(result2.has_value());
  ASSERT_TRUE(result2->is_error());
  ASSERT_TRUE(result2->error_value().is_framework_error()) << result2->error_value();
  EXPECT_TRUE(result2->error_value().framework_error().is_peer_closed()) << result2->error_value();
  EXPECT_EQ(result2->error_value().framework_error().status(), ZX_ERR_BAD_STATE)
      << result2->error_value();
}

TEST_F(ProcessLimboManagerTest, ManyHandlers) {
  ProcessLimboManager limbo_manager;

  std::vector<fidl::Client<ProcessLimbo>> clients;
  clients.push_back(Connect(&limbo_manager));
  clients.push_back(Connect(&limbo_manager));
  clients.push_back(Connect(&limbo_manager));

  // Calling each handler should be call the callback immediatelly.
  for (fidl::Client<ProcessLimbo>& client : clients) {
    std::optional<WatchActiveResult> result;
    client->WatchActive().Then([&result](WatchActiveResult& res) { result = std::move(res); });
    RunLoopUntilIdle();

    ASSERT_TRUE(result.has_value());
    ASSERT_TRUE(result->is_ok()) << result->error_value();
    ASSERT_FALSE(result->value().is_active());
  }

  // Calling again should not return.
  std::vector<bool> active_callbacks;
  for (fidl::Client<ProcessLimbo>& client : clients) {
    client->WatchActive().Then([&active_callbacks](WatchActiveResult& result) {
      ASSERT_TRUE(result.is_ok()) << result.error_value();
      active_callbacks.push_back(result.value().is_active());
    });
  }
  RunLoopUntilIdle();
  ASSERT_EQ(active_callbacks.size(), 0u);

  // Not changing the state should not issue any callbacks.
  ASSERT_FALSE(limbo_manager.SetActive(false));
  RunLoopUntilIdle();
  ASSERT_EQ(active_callbacks.size(), 0u);

  // Having a handler issue a SetActive command should trigger the callbacks.
  {
    std::optional<fidl::Result<ProcessLimbo::SetActive>> result;
    clients[0]->SetActive({true}).Then(
        [&result](fidl::Result<ProcessLimbo::SetActive>& res) { result = std::move(res); });
    RunLoopUntilIdle();

    ASSERT_TRUE(result.has_value());
    ASSERT_TRUE(result->is_ok()) << result->error_value();
  }

  ASSERT_EQ(active_callbacks.size(), 3u);
  EXPECT_TRUE(active_callbacks[0]);
  EXPECT_TRUE(active_callbacks[1]);
  EXPECT_TRUE(active_callbacks[2]);
}

TEST_F(ProcessLimboManagerTest, Filters) {
  ProcessLimboManager limbo_manager;

  // Override how the manager gets the process name.
  std::string name_to_return;
  limbo_manager.set_obtain_process_name_fn(
      [&name_to_return](zx_handle_t) -> std::string { return name_to_return; });

  // No filters should add the exception.
  name_to_return = "some-process";

  constexpr zx_koid_t kProcessKoid1 = 1;
  limbo_manager.AddToLimbo(GetFakeException(kProcessKoid1));

  // It should've added the exception.
  const std::map<zx_koid_t, ProcessException>& limbo = limbo_manager.limbo();
  ASSERT_EQ(limbo.size(), 1u);
  ASSERT_TRUE(limbo.contains(kProcessKoid1));

  // Adding a filter should filter out.
  limbo_manager.AppendFiltersForTesting({"filter"});

  constexpr zx_koid_t kProcessKoid2 = 2;
  limbo_manager.AddToLimbo(GetFakeException(kProcessKoid2));

  // No match, so the process should've been added.
  ASSERT_EQ(limbo.size(), 2u);
  EXPECT_TRUE(limbo.contains(kProcessKoid1));
  EXPECT_TRUE(limbo.contains(kProcessKoid2));

  // Adding a match should not append the process.
  name_to_return = "some-filtered-process";
  constexpr zx_koid_t kProcessKoid3 = 3;
  limbo_manager.AddToLimbo(GetFakeException(kProcessKoid3));

  ASSERT_EQ(limbo.size(), 2u);
  EXPECT_TRUE(limbo.contains(kProcessKoid1));
  EXPECT_TRUE(limbo.contains(kProcessKoid2));
}

TEST_F(ProcessLimboManagerTest, FiltersGetSet) {
  ProcessLimboManager limbo_manager;
  fidl::Client<ProcessLimbo> client = Connect(&limbo_manager);

  // We add some initial filters.
  limbo_manager.AppendFiltersForTesting({"filter-1", "filter-2"});

  // Get the initial watch.
  {
    std::optional<GetFiltersResult> result;
    client->GetFilters().Then([&result](GetFiltersResult& res) { result = std::move(res); });
    RunLoopUntilIdle();

    ASSERT_TRUE(result.has_value());
    ASSERT_TRUE(result->is_ok()) << result->error_value();
    const std::vector<std::string>& filters = result->value().filters();
    ASSERT_EQ(filters.size(), 2u);
    EXPECT_EQ(filters[0], "filter-1");
    EXPECT_EQ(filters[1], "filter-2");
  }

  // Make the hanging get.
  {
    std::optional<fidl::Result<ProcessLimbo::AppendFilters>> append_result;
    client->AppendFilters({std::vector<std::string>{"filter-3", "filter-4"}})
        .Then([&append_result](fidl::Result<ProcessLimbo::AppendFilters>& res) {
          append_result = std::move(res);
        });
    RunLoopUntilIdle();

    ASSERT_TRUE(append_result.has_value());
    ASSERT_TRUE(append_result->is_ok()) << append_result->error_value();

    std::optional<GetFiltersResult> result;
    client->GetFilters().Then([&result](GetFiltersResult& res) { result = std::move(res); });
    RunLoopUntilIdle();

    // The get should've been called.
    ASSERT_TRUE(result.has_value());
    ASSERT_TRUE(result->is_ok()) << result->error_value();
    const std::vector<std::string>& filters = result->value().filters();
    ASSERT_EQ(filters.size(), 4u);
    EXPECT_EQ(filters[0], "filter-1");
    EXPECT_EQ(filters[1], "filter-2");
    EXPECT_EQ(filters[2], "filter-3");
    EXPECT_EQ(filters[3], "filter-4");
  }

  // Removing some filters should call the hanging get too.
  {
    std::optional<fidl::Result<ProcessLimbo::RemoveFilters>> remove_result;
    client->RemoveFilters({std::vector<std::string>{"filter-1", "filter-3"}})
        .Then([&remove_result](fidl::Result<ProcessLimbo::RemoveFilters>& res) {
          remove_result = std::move(res);
        });
    RunLoopUntilIdle();

    ASSERT_TRUE(remove_result.has_value());
    ASSERT_TRUE(remove_result->is_ok()) << remove_result->error_value();

    std::optional<GetFiltersResult> result;
    client->GetFilters().Then([&result](GetFiltersResult& res) { result = std::move(res); });
    RunLoopUntilIdle();

    // The get should've been called.
    ASSERT_TRUE(result.has_value());
    ASSERT_TRUE(result->is_ok()) << result->error_value();
    const std::vector<std::string>& filters = result->value().filters();
    ASSERT_EQ(filters.size(), 2u);
    EXPECT_EQ(filters[0], "filter-2");
    EXPECT_EQ(filters[1], "filter-4");
  }
}

TEST_F(ProcessLimboManagerTest, DisablingFrees) {
  ProcessLimboManager limbo_manager;
  fidl::Client<ProcessLimbo> client = Connect(&limbo_manager);

  limbo_manager.SetActive(true);

  // We create multiple exceptions.
  ExceptionContext excps[3];
  ASSERT_TRUE(RetrieveExceptionContext(excps + 0));
  ASSERT_TRUE(RetrieveExceptionContext(excps + 1));
  ASSERT_TRUE(RetrieveExceptionContext(excps + 2));

  // Get the fidl representation of the exception.
  ExceptionInfo infos[3];
  infos[0] = ExceptionContextToExceptionInfo(excps[0]);
  infos[1] = ExceptionContextToExceptionInfo(excps[1]);
  infos[2] = ExceptionContextToExceptionInfo(excps[2]);

  AddExceptionToLimbo(&limbo_manager, std::move(excps[0].exception), infos[0]);
  AddExceptionToLimbo(&limbo_manager, std::move(excps[1].exception), infos[1]);
  AddExceptionToLimbo(&limbo_manager, std::move(excps[2].exception), infos[2]);

  // The exceptions should be there.
  ASSERT_EQ(limbo_manager.limbo().size(), 3u);

  {
    std::optional<fidl::Result<ProcessLimbo::SetActive>> result;
    client->SetActive({false}).Then(
        [&result](fidl::Result<ProcessLimbo::SetActive>& res) { result = std::move(res); });
    RunLoopUntilIdle();

    ASSERT_TRUE(result.has_value());
    ASSERT_TRUE(result->is_ok()) << result->error_value();
    ASSERT_EQ(limbo_manager.limbo().size(), 0u);
  }

  // We kill the jobs. This kills the underlying process. We do this so that the crashed process
  // doesn't get rescheduled. Otherwise the exception on the crash program would bubble out of our
  // environment and create noise on the overall system.
  excps[0].job.kill();
  excps[1].job.kill();
  excps[2].job.kill();
}

}  // namespace
}  // namespace exceptions
}  // namespace forensics
