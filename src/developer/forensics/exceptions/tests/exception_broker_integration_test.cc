// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#include <fidl/fuchsia.exception/cpp/fidl.h>
#include <fidl/fuchsia.feedback.testing/cpp/fidl.h>
#include <lib/sys/cpp/service_directory.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/syscalls.h>

#include <gtest/gtest.h>

#include "src/developer/forensics/exceptions/tests/crasher_wrapper.h"

namespace forensics {
namespace exceptions {
namespace {

using fuchsia_exception::ExceptionInfo;
using fuchsia_exception::ExceptionType;

bool GetExceptionContext(ExceptionContext* pe) {
  // Create a process that crashes and obtain the relevant handles and exception.
  // By the time |SpawnCrasher| has returned, the process has already thrown an exception.
  if (!SpawnCrasher(pe))
    return false;

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

TEST(ExceptionBrokerIntegrationTest, OnExceptionSmokeTest) {
  constexpr size_t kNumExceptions = 50;
  std::vector<ExceptionContext> exceptions(kNumExceptions);

  auto [handler_client, handler_server] = fidl::Endpoints<fuchsia_exception::Handler>::Create();

  std::shared_ptr<sys::ServiceDirectory> environment_services =
      sys::ServiceDirectory::CreateFromNamespace();
  environment_services->Connect(fidl::DiscoverableProtocolName<fuchsia_exception::Handler>,
                                handler_server.TakeChannel());
  fidl::SyncClient exception_handler(std::move(handler_client));

  for (size_t i = 0; i < kNumExceptions; ++i) {
    ExceptionContext& exception = exceptions[i];
    ASSERT_TRUE(GetExceptionContext(&exception));

    const fidl::Result result = exception_handler->OnException({{
        .exception = std::move(exception.exception),
        .info = ExceptionContextToExceptionInfo(exception),
    }});
    ASSERT_TRUE(result.is_ok()) << result.error_value();
  }

  auto [querier_client, querier_server] =
      fidl::Endpoints<fuchsia_feedback_testing::FakeCrashReporterQuerier>::Create();
  environment_services->Connect(
      fidl::DiscoverableProtocolName<fuchsia_feedback_testing::FakeCrashReporterQuerier>,
      querier_server.TakeChannel());
  fidl::SyncClient crash_reporter(std::move(querier_client));

  size_t num_crashreports{0};
  // Depending on how fast exception handling happens for each of the 5 exceptions, there might be
  // up to 6 calls to WatchFile() needed to get to the 5 filed crash reports.
  for (size_t i = 0; i < kNumExceptions + 1; ++i) {
    const fidl::Result result = crash_reporter->WatchFile();
    ASSERT_TRUE(result.is_ok()) << result.error_value();
    num_crashreports = result->num_filed();
    if (num_crashreports == kNumExceptions) {
      break;
    }
  }

  EXPECT_EQ(num_crashreports, kNumExceptions);

  for (ExceptionContext& exception : exceptions) {
    // Kill the job so that the exception that will be freed here doesn't bubble out of the test
    // environment.
    exception.job.kill();
  }
}

TEST(ExceptionBrokerIntegrationTest, GetProcessesOnExceptionSmokeTest) {
  ExceptionContext pe;
  ASSERT_TRUE(GetExceptionContext(&pe));

  auto [limbo_client, limbo_server] = fidl::Endpoints<fuchsia_exception::ProcessLimbo>::Create();

  std::shared_ptr<sys::ServiceDirectory> environment_services =
      sys::ServiceDirectory::CreateFromNamespace();
  environment_services->Connect(fidl::DiscoverableProtocolName<fuchsia_exception::ProcessLimbo>,
                                limbo_server.TakeChannel());
  fidl::SyncClient limbo(std::move(limbo_client));

  const fidl::Result result = limbo->WatchProcessesWaitingOnException();
  ASSERT_TRUE(result.is_ok() || result.error_value().is_domain_error()) << result.error_value();

  // Kill the job so that the exception that will be freed here doesn't bubble out of the test
  // environment.
  pe.job.kill();
}

}  // namespace
}  // namespace exceptions
}  // namespace forensics
