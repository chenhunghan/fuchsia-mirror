// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/handlers/request_async_backtrace.h"

#include <gtest/gtest.h>

#include "src/developer/debug/zxdb/client/mock_async_task.h"
#include "src/developer/debug/zxdb/client/mock_frame.h"
#include "src/developer/debug/zxdb/client/process.h"
#include "src/developer/debug/zxdb/client/thread.h"
#include "src/developer/debug/zxdb/common/scoped_temp_file.h"
#include "src/developer/debug/zxdb/debug_adapter/context_test.h"
#include "src/developer/debug/zxdb/symbols/compile_unit.h"
#include "src/developer/debug/zxdb/symbols/dwarf_lang.h"
#include "src/developer/debug/zxdb/symbols/function.h"
#include "src/developer/debug/zxdb/symbols/location.h"
#include "src/developer/debug/zxdb/symbols/symbol_context.h"
#include "src/developer/debug/zxdb/symbols/symbol_test_parent_setter.h"

namespace zxdb {

namespace {

class RequestAsyncBacktraceTest : public DebugAdapterContextTest {
 public:
  void SetUp() override {
    DebugAdapterContextTest::SetUp();
    temp_file_ = std::make_unique<ScopedTempFile>();
    fake_file_path_ = temp_file_->name();
  }

  std::unique_ptr<ScopedTempFile> temp_file_;
  std::string fake_file_path_;
};

TEST_F(RequestAsyncBacktraceTest, StoppedThreadWithAsyncExecutor) {
  InitializeDebugging();

  Process* process = InjectProcessWithModule(kProcessKoid, 0x1000);
  process->AddAsyncTaskProviderForTesting(ExprLanguage::kRust,
                                          std::make_unique<MockAsyncTaskProvider>(fake_file_path_));

  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  std::vector<std::unique_ptr<Frame>> frames;
  auto cu = fxl::MakeRefCounted<CompileUnit>(DwarfTag::kCompileUnit, fxl::WeakPtr<ModuleSymbols>(),
                                             fxl::RefPtr<DwarfUnit>(), DwarfLang::kRust, "test.rs",
                                             std::optional<uint64_t>());
  auto func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  func->set_assigned_name("executor");
  SymbolTestParentSetter parent_setter(func, cu);
  Location loc(0x1234, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, loc, 0));

  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSingleStep,
                           std::move(frames), true);

  dap::ZxdbAsyncBacktraceRequest req;
  req.threadId = static_cast<dap::integer>(kThreadKoid);
  auto response = client().send(req);

  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  auto got = response.get();
  EXPECT_FALSE(got.error);
  ASSERT_EQ(got.response.tasks.size(), 2u);

  ASSERT_TRUE(got.response.tasks[0].id.has_value());
  EXPECT_EQ(got.response.tasks[0].id.value(), "0x1");
  EXPECT_EQ(got.response.tasks[0].name, "root");
  EXPECT_TRUE(got.response.tasks[0].file.has_value());
  EXPECT_EQ(got.response.tasks[0].file.value(), fake_file_path_);
  EXPECT_TRUE(got.response.tasks[0].line.has_value());
  EXPECT_EQ(got.response.tasks[0].line.value(), 42);

  ASSERT_EQ(got.response.tasks[0].children.size(), 1u);
  ASSERT_TRUE(got.response.tasks[0].children[0].id.has_value());
  EXPECT_EQ(got.response.tasks[0].children[0].id.value(), "0x2");
  EXPECT_EQ(got.response.tasks[0].children[0].name, "child");

  ASSERT_EQ(got.response.tasks[0].children[0].children.size(), 1u);
  ASSERT_TRUE(got.response.tasks[0].children[0].children[0].id.has_value());
  EXPECT_EQ(got.response.tasks[0].children[0].children[0].id.value(), "0x3");
  EXPECT_EQ(got.response.tasks[0].children[0].children[0].name, "grandchild");

  EXPECT_FALSE(got.response.tasks[1].id.has_value());
  EXPECT_EQ(got.response.tasks[1].name, "zero_id_task");
}

TEST_F(RequestAsyncBacktraceTest, StoppedThreadWithoutAsyncExecutor) {
  InitializeDebugging();

  InjectProcessWithModule(kProcessKoid, 0x1000);
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  // Stack without any async task provider matching it.
  std::vector<std::unique_ptr<Frame>> frames;
  Location loc(0x1234, FileLine(), 0, SymbolContext::ForRelativeAddresses());
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, loc, 0));

  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSingleStep,
                           std::move(frames), true);

  dap::ZxdbAsyncBacktraceRequest req;
  req.threadId = static_cast<dap::integer>(kThreadKoid);
  auto response = client().send(req);

  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  auto got = response.get();
  EXPECT_FALSE(got.error);
  EXPECT_TRUE(got.response.tasks.empty());
}

TEST_F(RequestAsyncBacktraceTest, InvalidThreadId) {
  InitializeDebugging();

  {
    dap::ZxdbAsyncBacktraceRequest req;
    req.threadId = 0;
    auto response = client().send(req);

    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = response.get();
    EXPECT_TRUE(got.error);
    EXPECT_EQ(got.error.message, "Thread ID must be positive");
  }

  {
    dap::ZxdbAsyncBacktraceRequest req;
    req.threadId = -5;
    auto response = client().send(req);

    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = response.get();
    EXPECT_TRUE(got.error);
    EXPECT_EQ(got.error.message, "Thread ID must be positive");
  }
}

TEST_F(RequestAsyncBacktraceTest, ThreadNotFound) {
  InitializeDebugging();

  InjectProcess(kProcessKoid);
  RunPendingClientCalls();

  dap::ZxdbAsyncBacktraceRequest req;
  req.threadId = 999999;
  auto response = client().send(req);

  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  auto got = response.get();
  EXPECT_TRUE(got.error);
  EXPECT_EQ(got.error.message, "Thread not found");
}

TEST_F(RequestAsyncBacktraceTest, BlockedThreadNotSupportingFrames) {
  InitializeDebugging();

  InjectProcess(kProcessKoid);
  RunClient();
  InjectThread(kProcessKoid, kThreadKoid);
  RunClient();

  debug_ipc::NotifyException notification;
  notification.type = debug_ipc::ExceptionType::kNone;
  notification.thread.id = {.process = kProcessKoid, .thread = kThreadKoid};
  notification.thread.state = debug_ipc::ThreadRecord::State::kBlocked;
  notification.thread.blocked_reason = debug_ipc::ThreadRecord::BlockedReason::kSleeping;
  InjectExceptionWithStack(notification, {}, true);

  dap::ZxdbAsyncBacktraceRequest req;
  req.threadId = static_cast<dap::integer>(kThreadKoid);
  auto response = client().send(req);

  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  auto got = response.get();
  EXPECT_TRUE(got.error);
  EXPECT_EQ(got.error.message, "All threads must be stopped before requesting async-backtrace.");
}

TEST_F(RequestAsyncBacktraceTest, RunningThread) {
  InitializeDebugging();

  InjectProcess(kProcessKoid);
  InjectThread(kProcessKoid, kThreadKoid);

  // Thread is running (not stopped).
  dap::ZxdbAsyncBacktraceRequest req;
  req.threadId = static_cast<dap::integer>(kThreadKoid);
  auto response = client().send(req);

  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  auto got = response.get();
  EXPECT_TRUE(got.error);
  EXPECT_EQ(got.error.message, "All threads must be stopped before requesting async-backtrace.");
}

TEST_F(RequestAsyncBacktraceTest, MultipleThreadsRequiresAllThreadsStopped) {
  InitializeDebugging();

  Process* process = InjectProcessWithModule(kProcessKoid, 0x1000);
  process->AddAsyncTaskProviderForTesting(ExprLanguage::kRust,
                                          std::make_unique<MockAsyncTaskProvider>(fake_file_path_));

  constexpr uint64_t kThreadKoid1 = kThreadKoid;
  constexpr uint64_t kThreadKoid2 = kThreadKoid + 1;

  Thread* thread1 = InjectThread(kProcessKoid, kThreadKoid1);
  Thread* thread2 = InjectThread(kProcessKoid, kThreadKoid2);

  auto cu = fxl::MakeRefCounted<CompileUnit>(DwarfTag::kCompileUnit, fxl::WeakPtr<ModuleSymbols>(),
                                             fxl::RefPtr<DwarfUnit>(), DwarfLang::kRust, "test.rs",
                                             std::optional<uint64_t>());
  auto func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  func->set_assigned_name("executor");
  SymbolTestParentSetter parent_setter(func, cu);
  Location loc(0x1234, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);

  // Stop thread1 with an async executor while thread2 remains running.
  std::vector<std::unique_ptr<Frame>> frames1;
  frames1.push_back(std::make_unique<MockFrame>(&session(), thread1, loc, 0));
  InjectExceptionWithStack(kProcessKoid, kThreadKoid1, debug_ipc::ExceptionType::kSingleStep,
                           std::move(frames1), true);

  {
    dap::ZxdbAsyncBacktraceRequest req;
    req.threadId = static_cast<dap::integer>(kThreadKoid1);
    auto response = client().send(req);

    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = response.get();
    EXPECT_TRUE(got.error);
    EXPECT_EQ(got.error.message, "All threads must be stopped before requesting async-backtrace.");
  }

  // Stop thread2 without an async executor so all threads in the process are now stopped.
  std::vector<std::unique_ptr<Frame>> frames2;
  Location non_async_loc(0x5678, FileLine(), 0, SymbolContext::ForRelativeAddresses());
  frames2.push_back(std::make_unique<MockFrame>(&session(), thread2, non_async_loc, 0));
  InjectExceptionWithStack(kProcessKoid, kThreadKoid2, debug_ipc::ExceptionType::kSingleStep,
                           std::move(frames2), true);

  // Requesting async backtrace for thread1 should now succeed and return tasks.
  {
    dap::ZxdbAsyncBacktraceRequest req;
    req.threadId = static_cast<dap::integer>(kThreadKoid1);
    auto response = client().send(req);

    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = response.get();
    EXPECT_FALSE(got.error);
    ASSERT_EQ(got.response.tasks.size(), 2u);
    EXPECT_EQ(got.response.tasks[0].name, "root");
    EXPECT_EQ(got.response.tasks[1].name, "zero_id_task");
  }

  // Requesting async backtrace for thread2 should succeed with an empty task list.
  {
    dap::ZxdbAsyncBacktraceRequest req;
    req.threadId = static_cast<dap::integer>(kThreadKoid2);
    auto response = client().send(req);

    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = response.get();
    EXPECT_FALSE(got.error);
    EXPECT_TRUE(got.response.tasks.empty());
  }
}

}  // namespace

}  // namespace zxdb
